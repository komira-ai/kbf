//! One script of reads and a write, said line by line, run on the data plane the daemon builds
//! without the flag (`farm_over`, the production construction, over an in-process frontend) and
//! on the tierstore plane over a tierstore that serves nothing. The transcripts are the
//! golden's, byte for byte; the golden was taken from the previous implementation (main's
//! `farm`, over the same in-process frontend), not from this branch.

use std::fmt::Write as _;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use kbf_front::{Cache, MemoryMetaLog, VerifiedBlob};
use tonic::codegen::{Service, http::Uri};
use tonic::transport::{Channel, Endpoint, Server};

use super::*;

/// The frontend cache, in memory.
type Mem = Cache<MemoryMetaLog, MemoryStore>;

/// The inputs the script reads, which the frontend holds.
const INPUTS: [&[u8]; 2] = [b"input one", b"input two"];

/// The transcript's line for `result`: the bytes as text, or the error.
fn said<T: AsRef<[u8]>>(result: &Result<T, CasError>) -> String {
    match result {
        Ok(bytes) => format!("Ok({:?})", String::from_utf8_lossy(bytes.as_ref())),
        Err(error) => format!("Err({error:?})"),
    }
}

/// A connector that dials the frontend in this process: every dial is a fresh in-memory pipe
/// whose far end goes to the server's incoming stream.
#[derive(Clone)]
struct Pipes(tokio::sync::mpsc::UnboundedSender<tokio::io::DuplexStream>);

impl Service<Uri> for Pipes {
    type Response = hyper_util::rt::TokioIo<tokio::io::DuplexStream>;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = io::Result<Self::Response>> + Send>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _: Uri) -> Self::Future {
        let (near, far) = tokio::io::duplex(1 << 20);
        let sent = self.0.send(far);
        Box::pin(async move {
            sent.map_err(|_| io::Error::other("the frontend is gone"))?;
            Ok(hyper_util::rt::TokioIo::new(near))
        })
    }
}

/// A channel to the frontend over `cache`, served in this process through in-memory pipes.
fn frontend(cache: &Arc<Mem>) -> Channel {
    let (dials, incoming) = tokio::sync::mpsc::unbounded_channel();
    let incoming = futures::stream::unfold(incoming, |mut rx| async move {
        rx.recv().await.map(|pipe| (Ok::<_, io::Error>(pipe), rx))
    });
    let routes = kbf_front::routes(Arc::clone(cache));
    tokio::spawn(
        Server::builder()
            .add_routes(routes)
            .serve_with_incoming(incoming),
    );
    Endpoint::from_static("http://in-process").connect_with_connector_lazy(Pipes(dials))
}

/// A frontend cache holding the inputs.
async fn seeded() -> Arc<Mem> {
    let cache = Arc::new(Cache::memory());
    let blobs = INPUTS
        .iter()
        .map(|bytes| VerifiedBlob::hashed(Bytes::from_static(bytes)))
        .collect();
    cache.store_blobs(blobs).await.expect("seeded");
    cache
}

/// The daemon's plane without the flag, as `farm_over` makes it, over the engine to `cache`.
fn production(dir: &Path, cache: &Arc<Mem>) -> Arc<kbf_daemon::FarmCas> {
    tls_files(dir);
    let cli = daemon(dir, &[]);
    let liveness = cli.daemon.storage_liveness().expect("liveness");
    let server = Arc::new(kbf_transfer::Transfers::over(
        frontend(cache),
        cli.transfer.config(),
    ));
    let maps = Arc::new(MapHolder::new());
    crate::farm_over(&cli, &maps, liveness, server)
        .expect("the plane")
        .0
}

/// `plane` read and written by the script, over the frontend's `cache`, with scratch files in
/// `dir`; what it said.
async fn script<C: Cas>(plane: &Arc<C>, cache: &Arc<Mem>, dir: &Path) -> String {
    let mut out = String::new();
    let inputs: Vec<Digest> = INPUTS.iter().map(|bytes| digest_of(bytes)).collect();
    let absent = digest_of(b"absent");
    let put = plane.put(b"an output".to_vec()).await;
    let _ = writeln!(
        out,
        "put an output -> {}",
        put.map(|d| label(&d)).unwrap_or_else(|e| format!("{e:?}"))
    );
    let asked = [inputs[0].clone(), absent.clone(), digest_of(b"an output")];
    let missing: Vec<String> = plane.missing(&asked).await.iter().map(label).collect();
    let _ = writeln!(out, "missing -> {missing:?}");
    for digest in inputs.iter().chain([&absent]) {
        let _ = writeln!(
            out,
            "get {} -> {}",
            label(digest),
            said(&plane.get(digest).await)
        );
        let path = dir.join("read");
        let mut file = tokio::fs::File::create(&path).await.expect("create");
        file.write_all(b"head:").await.expect("head");
        file.flush().await.expect("flush");
        let read = plane.read_to(digest, &mut file).await;
        let held = std::fs::read(&path).expect("read back");
        let _ = writeln!(
            out,
            "read_to {} -> {}, file {:?}",
            label(digest),
            said(&read.map(|()| b"")),
            String::from_utf8_lossy(&held)
        );
    }
    let lease = plane.lease(&kbf_types::LeaseId::new(3, 4));
    let _ = writeln!(
        out,
        "lease get {} -> {}",
        label(&inputs[0]),
        said(&lease.get(&inputs[0]).await)
    );
    let stored = VerifiedBlob::hashed(Bytes::from_static(b"an output")).digest();
    let stored = cache.read_blob(&stored).await;
    let _ = writeln!(
        out,
        "the frontend holds the output -> {}",
        stored.map_or_else(
            |e| format!("Err({e:?})"),
            |b| format!("Ok({:?})", String::from_utf8_lossy(b.as_ref()))
        )
    );
    out
}

/// The golden: the transcript of the plane as main's code builds it over the same frontend,
/// taken from main before this change (the merge base of the branch).
const GOLDEN: &str = include_str!("../../../../tests/golden/plane-without-tierstore.txt");

/// Asserts that the plane `farm_over` builds, which is the daemon's without the flag, over an
/// in-process frontend, says the golden transcript byte for byte: every answer, every byte
/// that reaches a file, the write and what the frontend ends up holding.
/// Catches a change to the daemon's own data plane that the flag's absence does not undo.
#[tokio::test]
async fn without_the_flag_the_production_plane_says_mains_transcript() {
    let dir = scratch("tierstore-transcript-plain");
    let cache = seeded().await;
    let plane = production(&dir, &cache);
    assert_eq!(script(&plane, &cache, &dir).await, GOLDEN);
    kbf_outputs::remove_tree(&dir).expect("clean");
}

/// How the tierstore of a run of the script fails to serve the inputs.
#[derive(Clone, Copy, Debug)]
enum Unserved {
    /// It is down.
    Down,
    /// It does not have their packs.
    Missing,
    /// It holds other bytes where their packs are.
    Damaged,
}

/// Asserts that the production plane wrapped by the tierstore reads, over a tierstore that
/// serves none of the inputs (down, missing their packs, or holding damaged copies), says the
/// golden transcript byte for byte: every answer, every byte that reaches a file, the write
/// and what the frontend holds are the same as without the flag. Each run did ask the
/// tierstore.
/// Catches a tierstore plane whose degraded behaviour differs from the daemon's without it.
#[tokio::test]
async fn over_a_tierstore_that_serves_nothing_the_production_plane_says_the_same_transcript() {
    for unserved in [Unserved::Down, Unserved::Missing, Unserved::Damaged] {
        let dir = scratch(&format!("tierstore-transcript-{unserved:?}"));
        let rig = Rig::new();
        for (i, input) in INPUTS.iter().enumerate() {
            let key = format!("cas/d/node1/boot1/{:016x}", i + 1);
            let stored = match unserved {
                Unserved::Down => Some(*input),
                Unserved::Missing => None,
                Unserved::Damaged => Some(if i == 0 {
                    b"input 0ne".as_slice()
                } else {
                    b"input tw0"
                }),
            };
            rig.record(&key, input, stored).await;
        }
        if matches!(unserved, Unserved::Down) {
            rig.tier.set(Mode::Down);
        }
        let cache = seeded().await;
        let under = production(&dir, &cache);
        let counter = PackCounter::default();
        let plane = over_placed(
            under,
            Arc::clone(&rig.tier),
            Arc::clone(&rig.placed),
            &counter,
        );
        assert_eq!(script(&plane, &cache, &dir).await, GOLDEN, "{unserved:?}");
        assert!(rig.reads() > 0, "{unserved:?}: the tierstore was not asked");
        kbf_outputs::remove_tree(&dir).expect("clean");
    }
}
