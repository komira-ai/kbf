//! How much memory the whole in-process path holds while one large output is stored:
//! the container driver's walk, the plane and the server's upload until its commit,
//! all in this process. A counting global allocator records the most bytes live at once
//! while each test runs, beyond those live when it started (the server is started
//! before). The outputs are sparse files, so they cost the test machine no disk to
//! create.
//!
//! The tests share the counters, so each runs alone ([`peak_during`]).

#![cfg(target_os = "linux")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use clap::Parser as _;
use kbf_all_in_one::LocalDataPlane;
use kbf_daemon::cpu_pool::CpuPool;
use kbf_driver_container::tree::{OutputLimits, collect};
use kbf_proto::reapi::ActionResult;
use kbf_transfer::Cas;
use kbf_types::LeaseId;

/// The walk's chunk (`kbf_driver_container::CHUNK`, 1 MiB).
const CHUNK: usize = 1 << 20;

/// The most bytes the whole path may hold live at once beyond what was live when the
/// test started: the chunk window (one chunk per hashing thread, nothing more on the
/// plane's or the server's side) and room for the runtimes' own allocations. A file
/// held whole anywhere on the path (2 GiB) is far past it.
const BOUND: usize = 16 * CHUNK;

/// Bytes allocated and not yet freed, and the most of them seen since the last reset.
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grew(by: usize) {
    let now = LIVE.fetch_add(by, Ordering::Relaxed) + by;
    PEAK.fetch_max(now, Ordering::Relaxed);
}

fn shrank(by: usize) {
    LIVE.fetch_sub(by, Ordering::Relaxed);
}

/// The system allocator, counted.
struct Counting;

// SAFETY: every call goes to `System` with the caller's arguments unchanged; the
// counters only observe the sizes.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller upholds `alloc`'s contract, which is `System.alloc`'s.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            grew(layout.size());
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: as for `alloc`.
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            grew(layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` came from this allocator, so from `System`, with `layout`.
        unsafe { System.dealloc(ptr, layout) };
        shrank(layout.size());
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: as for `dealloc`, and the caller upholds `realloc`'s contract.
        let new = unsafe { System.realloc(ptr, layout, new_size) };
        if !new.is_null() {
            grew(new_size);
            shrank(layout.size());
        }
        new
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Runs `f` with no other test of this binary running, and returns its result and the
/// most bytes live at once during it beyond those live when it started.
fn peak_during<T>(f: impl FnOnce() -> T) -> (T, usize) {
    static ALONE: Mutex<()> = Mutex::new(());
    let _alone = ALONE.lock().unwrap_or_else(PoisonError::into_inner);
    let base = LIVE.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let out = f();
    (out, PEAK.load(Ordering::Relaxed).saturating_sub(base))
}

/// Servers started by this binary so far: each directory is one server's alone.
static STARTED: AtomicU64 = AtomicU64::new(0);

/// A fresh, empty directory under Cargo's temporary directory for this target.
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("kbf-all-in-one-memory")
        .join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch directory");
    dir
}

/// The plane over a server started in `runtime` on the stores under `dir`, with a
/// write tier. The server is not served: its flusher, which reads a staged block back
/// whole to PUT it (a cost of the flush, after the commit), does not run while the
/// upload path is measured.
fn plane(
    runtime: &tokio::runtime::Runtime,
    dir: &Path,
) -> (Arc<LocalDataPlane>, kbf_server::ServerHandle) {
    let n = STARTED.fetch_add(1, Ordering::SeqCst);
    let argv = vec![
        "kbf-server".to_owned(),
        "--listen=127.0.0.1:0".to_owned(),
        "--worker-listen=127.0.0.1:0".to_owned(),
        format!(
            "--cold-store=file://{}",
            dir.join(format!("cold-{n}")).display()
        ),
        format!(
            "--hot-store=file://{}",
            dir.join(format!("hot-{n}")).display()
        ),
    ];
    let args = kbf_server::Args::try_parse_from(argv).expect("flags");
    runtime.block_on(async {
        let never = || Ok(std::future::pending::<()>());
        let server = kbf_server::start(&args, never).await.expect("start");
        let plane = LocalDataPlane::new(server.local(), tokio::runtime::Handle::current());
        (Arc::new(plane), server)
    })
}

/// Stores the sparse output `out/large` of `size` bytes through a lease of the plane
/// over a server with a write tier, and returns the most bytes live at once while it
/// did; checks it is held after.
fn store_one_large_output(name: &str, size: u64) -> usize {
    let dir = scratch(name);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let (plane, _server) = plane(&runtime, &dir.join("server"));
    let upper = dir.join("upper");
    std::fs::create_dir_all(upper.join("out")).expect("mkdir");
    std::fs::File::create(upper.join("out/large"))
        .and_then(|file| file.set_len(size))
        .expect("sparse file");
    let lease = plane.lease(&LeaseId::new(1, 1));
    let pool = CpuPool::new(4);
    let (result, peak) = peak_during(|| {
        let mut result = ActionResult::default();
        let collected = runtime.block_on(collect(
            &lease,
            &upper,
            &pool,
            "",
            &["out/large".to_owned()],
            OutputLimits::DEFAULT,
            &mut result,
        ));
        collected.map(|()| result)
    });
    let result = result.expect("collect");
    let digest = result.output_files[0].digest.clone().expect("a digest");
    let missing = runtime.block_on(lease.missing(&[digest]));
    assert!(missing.is_empty(), "not held");
    let _ = std::fs::remove_dir_all(&dir);
    peak
}

/// Catches a file held whole anywhere on the in-process path (the walk, the plane's
/// intake, the server's upload until its commit): a 2 GiB output streams onto the
/// write tier with no more than [`BOUND`] live at once.
#[test]
fn a_2_gib_output_streams_onto_the_tier_in_bounded_memory() {
    let peak = store_one_large_output("tier-2g", 2 << 30);
    assert!(peak < BOUND, "{peak} bytes live at once");
}
