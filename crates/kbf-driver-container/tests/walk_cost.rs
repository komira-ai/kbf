//! What the walk of an output tree costs per file: how many megabyte buffers it
//! allocates, and how long a tree the size of a cold toolchain unpack takes to walk,
//! hash and hand to the CAS. A counting global allocator, so alone in its test binary.
//!
//! A cold unpack of 15,189 files spent 5.4 s of its 7.8 s walk on a thread hand-off and
//! a freshly zeroed megabyte for every file, to hash files of a few kilobytes.

mod support;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use kbf_daemon::cpu_pool::CpuPool;
use kbf_driver_container::MemoryCas;
use kbf_driver_container::tree::{OutputLimits, collect};
use kbf_proto::reapi::ActionResult;

/// Allocations of a megabyte or more, ever (grown or zeroed ones included).
static LARGE_ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

fn noted(size: usize) {
    if size >= 1 << 20 {
        LARGE_ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
    }
}

struct Counting;

// SAFETY: every call goes to `System` with the caller's arguments unchanged; the counter
// only observes the sizes.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        noted(layout.size());
        // SAFETY: the caller upholds `alloc`'s contract, which is `System.alloc`'s.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        noted(layout.size());
        // SAFETY: as for `alloc`.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` came from this allocator, so from `System`, with `layout`.
        unsafe { System.dealloc(ptr, layout) };
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        noted(new_size);
        // SAFETY: as for `dealloc`, and the caller upholds `realloc`'s contract.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// The tests share the allocation counter, so each runs alone.
static ALONE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A tree of `files` small files, 100 to a directory, in a fresh directory.
fn tree(name: &str, files: usize) -> std::path::PathBuf {
    let upper = support::scratch(name);
    for dir in 0..files.div_ceil(100) {
        let path = upper.join(format!("out/d{dir:04}"));
        std::fs::create_dir_all(&path).expect("mkdir");
        for n in 0..100.min(files - dir * 100) {
            std::fs::write(
                path.join(format!("f{n:03}")),
                format!("file {dir} {n}\n").repeat(30),
            )
            .expect("write");
        }
    }
    upper
}

async fn walk(upper: &std::path::Path, threads: usize) -> (ActionResult, MemoryCas, Duration) {
    let cas = MemoryCas::new();
    let mut result = ActionResult::default();
    let started = Instant::now();
    collect(
        &cas,
        upper,
        &CpuPool::new(threads),
        "",
        &["out".to_owned()],
        OutputLimits::DEFAULT,
        &mut result,
    )
    .await
    .expect("collect");
    (result, cas, started.elapsed())
}

/// Catches a megabyte allocated per file: 3,000 files of about 400 bytes are walked,
/// hashed and stored with at most a handful of allocations of a megabyte or more, not
/// one per file (mutant: the hash buffer sized to a chunk for every file, or zeroed
/// afresh per file, as `FileBlob::hash` did: 3,000). The budget is the hashing threads'
/// buffers (none for files this small) and the run's own, with room for the runtime.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_walk_allocates_no_megabyte_buffer_per_file() {
    let _alone = ALONE.lock().await;
    let upper = tree("walk-cost-allocations", 3_000);
    let before = LARGE_ALLOCATIONS.load(Ordering::Relaxed);
    let (result, _, _) = walk(&upper, 4).await;
    let large = LARGE_ALLOCATIONS.load(Ordering::Relaxed) - before;
    assert!(result.output_directories[0].tree_digest.is_some());
    assert!(
        large <= 8,
        "{large} allocations of a megabyte or more for 3,000 files"
    );
}

/// Catches a walk that costs far more per file than it should, as a guard against a
/// return of a per-file hand-off to another thread: 15,189 files, the number of a cold
/// toolchain unpack, are walked, hashed on 4 threads and stored in memory within 60 s on
/// a shared CI machine (the old walk spent more than 5 s on its per-file overhead alone, on a
/// fast node). The tree is the same with 1 thread and with
/// 8: the digests do not depend on how many threads hashed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tree_of_fifteen_thousand_files_is_walked_in_bounded_time() {
    let _alone = ALONE.lock().await;
    let upper = tree("walk-cost-fifteen-thousand", 15_189);
    let (result, cas, took) = walk(&upper, 4).await;
    assert!(took < Duration::from_secs(60), "{took:?} for 15,189 files");
    let tree = result.output_directories[0]
        .tree_digest
        .clone()
        .expect("tree digest");
    assert!(cas.blob(&tree).is_some());
    for threads in [1, 8] {
        let (again, _, _) = walk(&upper, threads).await;
        assert_eq!(again, result, "{threads} threads");
    }
}
