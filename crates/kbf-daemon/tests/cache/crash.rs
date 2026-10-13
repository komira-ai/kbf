//! The input cache after a crash (`docs/design/worker-cache.md#68-crash-and-restart`):
//! a daemon killed with SIGKILL mid-fill, between two snapshots, and mid-snapshot. The
//! daemon is this test binary run again as a child (`CHILD`), which opens the cache,
//! does its part, says it is ready by a marker file at the moment to crash, and waits
//! there to be killed. Nothing of a clean stop runs in it.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::support::cache_kit::{
    CountingCas, FakeClock, Gates, config, digest, entries, entry_path, file, key, lease,
    on_disk_sum, stop,
};
use crate::support::scratch;
use kbf_daemon::cache::{Cache, Hooks, Point};

/// The argument after argv0 that makes this binary the crashing daemon.
const CHILD: &str = "__cache-crash-child";

const GIB: u64 = 1 << 30;

/// The files of set `name` (`a` or `b`): 20 blobs, all different.
fn set(name: &str) -> Vec<(String, Vec<u8>, bool)> {
    (0..20)
        .map(|i| {
            file(
                &format!("{name}/f{i}"),
                format!("set {name} file {i}").as_bytes(),
                false,
            )
        })
        .collect()
}

/// The child's hooks: the first pass is held for good (so no snapshot but the explicit
/// ones is written), and once armed, the point `crash_at` writes the marker and waits
/// to be killed.
struct Crash {
    marker: PathBuf,
    crash_at: fn(&Point<'_>) -> bool,
    armed: AtomicBool,
}

impl Hooks for Crash {
    fn at(&self, point: Point<'_>) {
        let crash = self.armed.load(Ordering::SeqCst) && (self.crash_at)(&point);
        if point == Point::PassStarts || crash {
            if crash {
                std::fs::write(&self.marker, b"ready").expect("marker");
            }
            loop {
                std::thread::sleep(Duration::from_secs(60));
            }
        }
    }
}

/// The child: `<mode> <dir>`. `fill` crashes at its first fill's rename; `sets` fills
/// set a, saves a snapshot, fills set b and crashes; `snapshot` does the same but
/// crashes halfway through writing the second snapshot.
fn child(mode: &str, dir: &Path) -> ! {
    let crash_at: fn(&Point<'_>) -> bool = match mode {
        "fill" => |p| matches!(p, Point::Filled(_)),
        "snapshot" => |p| *p == Point::SnapshotHalf,
        _ => |_| false,
    };
    let hooks = Arc::new(Crash {
        marker: dir.join("ready"),
        crash_at,
        armed: AtomicBool::new(mode == "fill"),
    });
    let mut config = config(dir, GIB, Gates::new(), &FakeClock::at(1000));
    config.hooks = Arc::clone(&hooks) as Arc<dyn Hooks>;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let cache = Cache::open(config).expect("open");
        let cas = CountingCas::default();
        if mode == "fill" {
            let root = cas.tree(&[file("f", b"crash me", false)]);
            let _ = lease(&cas, &cache, &root, dir, "fill").await;
        } else {
            lease(&cas, &cache, &cas.tree(&set("a")), dir, "a")
                .await
                .expect("set a");
            cache.write_snapshot().expect("snapshot of a");
            lease(&cas, &cache, &cas.tree(&set("b")), dir, "b")
                .await
                .expect("set b");
            hooks.armed.store(true, Ordering::SeqCst);
            if mode == "snapshot" {
                let _ = cache.write_snapshot();
            }
            std::fs::write(dir.join("ready"), b"ready").expect("marker");
        }
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

/// R-5. Catches an unindexed file served without a re-hash, and files inserted since
/// the last snapshot deleted or never counted (the "adopt as verified", "link an
/// existing name without hashing", "make the pass unlink unknown files" and "adopt
/// without adding the size" mutants). Set a is in the snapshot, set b was filled after
/// it when the daemon was killed; one b file then has a byte flipped. With the pass
/// held, a lease needing a good b file fetches nothing and re-hashes it; one needing
/// the flipped file re-hashes it, fetches it once and gets its bytes. The pass then
/// adopts every other b file, unverified, unlinking none, and the total is the on-disk
/// sum.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn files_filled_after_the_last_snapshot_are_adopted_after_a_re_hash() {
    let dir = scratch("cache-r5");
    crash("sets", &dir);
    let b = set("b");
    let flipped = entry_path(&dir, &key(&b[1].1, false));
    std::fs::set_permissions(&flipped, std::fs::Permissions::from_mode(0o644)).expect("chmod");
    let mut bytes = std::fs::read(&flipped).expect("read");
    bytes[0] ^= 1;
    std::fs::write(&flipped, &bytes).expect("flip");
    std::fs::set_permissions(&flipped, std::fs::Permissions::from_mode(0o444)).expect("chmod back");
    let before = entries(&dir);
    let gates = Gates::running();
    let pass = gates.hold(|p| *p == Point::PassStarts);
    let cache =
        Cache::open(config(&dir, GIB, Arc::clone(&gates), &FakeClock::at(2000))).expect("open");
    gates.wait_reached(pass);
    assert_eq!(cache.stats().index_discarded, None);
    let cas = CountingCas::default();
    let (_, good) = lease(&cas, &cache, &cas.tree(&b[..1]), &dir, "good")
        .await
        .expect("good");
    assert_eq!(
        (cas.file_bytes(), good.verify_hashes, good.cache_misses),
        (0, 1, 1)
    );
    let (at, bad) = lease(&cas, &cache, &cas.tree(&b[1..2]), &dir, "bad")
        .await
        .expect("bad");
    assert_eq!(std::fs::read(at.join("b/f1")).expect("read"), b[1].1);
    assert_eq!(cas.reads_of(&digest(&b[1].1)), 1);
    assert_eq!(
        bad.verify_hashes, 0,
        "a file that hashed wrong counted as verified"
    );
    let snapshots = cache.stats().snapshots;
    gates.release(pass);
    let deadline = Instant::now() + Duration::from_secs(30);
    while cache.stats().snapshots == snapshots {
        assert!(Instant::now() < deadline, "the pass did not end");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(cache.stats().adopted_entries, 18);
    let (_, rest) = lease(&cas, &cache, &cas.tree(&b[2..]), &dir, "rest")
        .await
        .expect("rest");
    assert_eq!(
        (
            rest.verify_hashes,
            rest.cache_hits,
            cas.reads_of(&digest(&b[5].1))
        ),
        (18, 18, 0)
    );
    let after = entries(&dir);
    for (name, (_, inode)) in &before {
        if !flipped.ends_with(name) {
            assert_eq!(
                after.get(name).map(|e| e.1),
                Some(*inode),
                "{name} was unlinked or replaced"
            );
        }
    }
    assert_eq!(cache.stats().bytes, on_disk_sum(&dir));
    stop(cache, &gates);
}

/// Catches a snapshot written in place (a crash mid-write would leave a mix of two
/// snapshots, discarded at the next start): a daemon killed halfway through writing its
/// second snapshot leaves the first one whole, which the next start loads.
#[tokio::test]
async fn a_daemon_killed_mid_snapshot_leaves_the_previous_one() {
    let dir = scratch("cache-snapshot-crash");
    crash("snapshot", &dir);
    assert!(
        dir.join("cache/v1/index.snap.tmp").exists(),
        "the crash was not mid-write"
    );
    let gates = Gates::running();
    let pass = gates.hold(|p| *p == Point::PassStarts);
    let cache =
        Cache::open(config(&dir, GIB, Arc::clone(&gates), &FakeClock::at(2000))).expect("open");
    gates.wait_reached(pass);
    assert_eq!(
        cache.stats().index_discarded,
        None,
        "the previous snapshot was not kept whole"
    );
    assert!(!dir.join("cache/v1/index.snap.tmp").exists());
    for (_, bytes, _) in set("a") {
        assert!(cache.holds(&key(&bytes, false)));
    }
    for (_, bytes, _) in set("b") {
        assert!(
            !cache.holds(&key(&bytes, false)),
            "a key from the half-written snapshot"
        );
    }
    gates.release(pass);
    stop(cache, &gates);
}
