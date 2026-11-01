//! The plane's warning that the packs could not be asked. Alone in its test binary: a tracing
//! callsite that another test's thread reached first, with no subscriber installed, is
//! cached as uninteresting, and a subscriber installed after would miss it.

mod support;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use kbf_daemon::cas::Cas as _;
use kbf_daemon::{PackReads, PackedCas};
use kbf_pack::{Fetch, ReadError};
use kbf_pack_index::{BlobDigest, PackIndexError};
use support::memory::MemoryCas;

/// Packs that fail every call while `failing` is set, and have no blob otherwise.
struct Flaky {
    failing: AtomicBool,
}

impl PackReads for Flaky {
    fn read_blobs<'a>(
        &'a self,
        digests: &'a [BlobDigest],
    ) -> BoxFuture<'a, Result<Vec<Fetch>, ReadError>> {
        let answer = if self.failing.load(Ordering::SeqCst) {
            Err(ReadError::Index(PackIndexError::NotMigrated))
        } else {
            Ok(digests.iter().map(|_| Fetch::Missing).collect())
        };
        Box::pin(std::future::ready(answer))
    }
}

/// Asserts that packs that cannot be asked are warned of at the first read that finds them so,
/// not at each read after it, and again at the first such read after one that was answered, and
/// that every read is still answered by the plane underneath and counted as unasked.
/// (A current-thread runtime, so the test's subscriber sees the plane's calls.)
#[tokio::test]
async fn packs_that_cannot_be_asked_are_warned_of_once_until_a_read_gets_through() {
    let log = Log::default();
    let _installed = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_writer(log.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .finish(),
    );
    let flaky = Arc::new(Flaky {
        failing: AtomicBool::new(true),
    });
    let packs: Arc<dyn PackReads> = flaky.clone();
    let underneath = Arc::new(MemoryCas::default());
    let digest = underneath.insert(b"held by the storage nodes");
    let cas = PackedCas::new(underneath, packs);
    let warning = "the packs could not be asked";

    for _ in 0..3 {
        cas.get(&digest).await.expect("read as before");
    }
    assert_eq!(log.count(warning), 1);

    flaky.failing.store(false, Ordering::SeqCst);
    cas.get(&digest).await.expect("read as before");
    assert_eq!(log.count(warning), 1);

    flaky.failing.store(true, Ordering::SeqCst);
    for _ in 0..2 {
        cas.get(&digest).await.expect("read as before");
    }
    assert_eq!(log.count(warning), 2);
    assert_eq!(cas.stats().unasked, 5);
}

/// What a subscriber wrote, for the test above.
#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<u8>>>);

impl Log {
    fn count(&self, needle: &str) -> usize {
        let text = String::from_utf8(self.0.lock().expect("lock").clone()).expect("UTF-8");
        text.matches(needle).count()
    }
}

impl std::io::Write for Log {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("lock").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Log {
    type Writer = Self;

    fn make_writer(&'a self) -> Self {
        self.clone()
    }
}
