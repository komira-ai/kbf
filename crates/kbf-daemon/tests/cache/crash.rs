//! The input cache after a crash (`docs/design/worker-cache.md#68-crash-and-restart`):
//! a daemon killed with SIGKILL mid-fill. The daemon is this test binary run again as
//! a child (`CHILD`), which opens the cache, fills, says it is ready by a marker file
//! at the moment to crash, and waits there to be killed. Nothing of a clean stop runs
//! in it.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::support::cache_kit::{
    CountingCas, FakeClock, Gates, config, digest, entry_path, file, key, lease, stop,
};
use crate::support::scratch;
use kbf_daemon::cache::{Cache, Hooks, Point};

/// The argument after argv0 that makes this binary the crashing daemon.
const CHILD: &str = "__cache-crash-child";

const GIB: u64 = 1 << 30;

/// The child's hooks: at its first fill's rename, write the marker and wait to be
/// killed.
struct Crash {
    marker: std::path::PathBuf,
}

impl Hooks for Crash {
    fn at(&self, point: Point<'_>) {
        if matches!(point, Point::Filled(_)) {
            std::fs::write(&self.marker, b"ready").expect("marker");
            loop {
                std::thread::sleep(Duration::from_secs(60));
            }
        }
    }
}

/// The child: `<mode> <dir>`; `fill` crashes at its first fill's rename.
fn child(mode: &str, dir: &Path) -> ! {
    assert_eq!(mode, "fill");
    let mut config = config(dir, GIB, Gates::new(), &FakeClock::at(1000));
    config.hooks = Arc::new(Crash {
        marker: dir.join("ready"),
    });
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let cache = Cache::open(config).expect("open");
        let cas = CountingCas::default();
        let root = cas.tree(&[file("f", b"crash me", false)]);
        let _ = lease(&cas, &cache, &root, dir, "fill").await;
    });
    loop {
        std::thread::sleep(Duration::from_secs(60));
    }
}

extern "C" fn crash_child_hook() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some(CHILD) {
        child(&args[2], Path::new(&args[3]));
    }
}

#[used]
#[unsafe(link_section = ".init_array")]
static CRASH_CHILD_HOOK: extern "C" fn() = crash_child_hook;

/// Runs the child in `mode` on `dir`, waits for its marker, and kills it with SIGKILL.
fn crash(mode: &str, dir: &Path) {
    let exe = std::env::current_exe().expect("this test binary");
    let mut child = Command::new(exe)
        .args([CHILD, mode])
        .arg(dir)
        .spawn()
        .expect("the child");
    let deadline = Instant::now() + Duration::from_secs(60);
    while !dir.join("ready").exists() {
        let exited = child.try_wait().expect("wait").is_some();
        if exited || Instant::now() >= deadline {
            // Killed either way, so a failing test leaves no child behind.
            let _ = child.kill();
            panic!("the child never got ready (exited: {exited})");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    child.kill().expect("SIGKILL");
    child.wait().expect("reaped");
    std::fs::remove_file(dir.join("ready")).expect("marker");
}

/// R-1. Catches a partial or unhashed file under a final name and a `tmp/` not cleared
/// (the "fill straight into `blobs/`" and "skip the `tmp/` clear" mutants): a daemon
/// killed between a fill's last write and its rename leaves its file in `tmp/` only;
/// the next start clears it, and the next lease fetches the blob and gets its bytes.
#[tokio::test]
async fn a_fill_killed_before_its_rename_leaves_no_entry() {
    let dir = scratch("cache-r1");
    crash("fill", &dir);
    let k = key(b"crash me", false);
    assert_eq!(
        std::fs::read_dir(dir.join("cache/v1/tmp"))
            .expect("tmp")
            .count(),
        1,
        "the crash left no fill"
    );
    assert!(!entry_path(&dir, &k).exists(), "an entry before its rename");
    let gates = Gates::new();
    let cache =
        Cache::open(config(&dir, GIB, Arc::clone(&gates), &FakeClock::at(2000))).expect("open");
    assert_eq!(
        std::fs::read_dir(dir.join("cache/v1/tmp"))
            .expect("tmp")
            .count(),
        0,
        "tmp/ not cleared"
    );
    let cas = CountingCas::default();
    let (at, _) = lease(
        &cas,
        &cache,
        &cas.tree(&[file("f", b"crash me", false)]),
        &dir,
        "after",
    )
    .await
    .expect("lease");
    assert_eq!(std::fs::read(at.join("f")).expect("read"), b"crash me");
    assert_eq!(cas.reads_of(&digest(b"crash me")), 1);
    stop(cache, &gates);
}
