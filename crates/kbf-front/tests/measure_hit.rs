//! A measurement, not a check: what an action-cache hit costs over a file store
//! against the memory store, as the number of distinct stored objects the hit needs
//! grows. `Cache::action_result` asks the store whether it may still hold each of
//! those objects, one after another, before it serves the hit; over `FsStore` each ask
//! is a blocking-pool hop plus an open, fstat and header read. This prints the numbers
//! that decide whether that loop is worth making concurrent: each hit's latency, the
//! `may_hold` loop alone, the cost per object, and the file/memory ratio.
//!
//! Nothing is asserted about the timings, so it is ignored and off the gate. It does
//! assert that every timed hit is a hit, over exactly the objects it claims, so it
//! cannot quietly time misses. Run in release, as the server ships:
//!
//! `cargo test -p kbf-front --test measure_hit --release -- --ignored --nocapture`

mod common;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use bytes::Bytes;
use common::Blob;
use kbf_front::{Cache, MemoryMetaLog, MetaLog, VerifiedBlob};
use kbf_meta::{Retention, Role};
use kbf_objstore::{Capabilities, Floor, FsStore, KeyPrefix, MemoryStore, ObjectStore};
use kbf_proto::reapi::{ActionResult, OutputFile};
use kbf_types::Digest;

/// Distinct stored objects one hit needs.
const OBJECTS_PER_HIT: [usize; 4] = [1, 4, 16, 64];
/// Distinct actions per run; the timed hits go round them.
const ACTIONS: usize = 16;
/// Timed hits per store and object count.
const HITS: usize = 10_000;
/// Bytes per output blob.
const OUTPUT_BYTES: usize = 4096;
/// A floor every file system the tests run on meets: one byte.
const FLOOR: Floor = Floor::Bytes(1);

/// A fresh, absent directory for `name`, under the target directory.
fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("kbf-front-measure-hit")
        .join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// One action's entry: its digest and the stored objects a hit on it needs.
struct Entry {
    action: Digest,
    objects: usize,
}

/// Writes `ACTIONS` entries to `cache`, each needing `objects` distinct stored
/// objects: `objects - 1` outputs, each stored on its own (so in its own segment),
/// plus the result blob, which `write_action_result` stores in one more. Checks the
/// index agrees on the object count, so a run labelled M really needs M objects.
async fn entries<M: MetaLog, O: ObjectStore>(
    cache: &Cache<M, O>,
    objects: usize,
    run: usize,
) -> Vec<Entry> {
    let mut out = Vec::with_capacity(ACTIONS);
    for a in 0..ACTIONS {
        let mut output_files = Vec::with_capacity(objects - 1);
        for o in 0..objects - 1 {
            // Even seeds only: `pseudo_random` sets the low bit, so 2k and 2k+1 match.
            let seed = u64::try_from(((run * ACTIONS + a) * 128 + o) * 2).expect("seed");
            let blob = Blob::new(common::pseudo_random(OUTPUT_BYTES, seed));
            cache
                .store_blobs(vec![VerifiedBlob::hashed(Bytes::from(blob.data))])
                .await
                .expect("store an output");
            output_files.push(OutputFile {
                path: format!("out/{o}"),
                digest: Some(blob.proto),
                ..Default::default()
            });
        }
        let action = Blob::new(format!("action {run} {a}")).digest;
        let result = ActionResult {
            output_files,
            exit_code: 0,
            ..Default::default()
        };
        cache
            .write_action_result(Role::Daemon, action, &result)
            .await
            .expect("write the entry");
        let held = cache
            .meta()
            .query(move |s| s.stored_objects(&action).len())
            .await
            .expect("query");
        assert_eq!(held, objects, "the entry needs {objects} distinct objects");
        out.push(Entry { action, objects });
    }
    out
}

/// Latencies of one series, in microseconds.
struct Series(Vec<f64>);

impl Series {
    fn new(mut v: Vec<Duration>) -> Self {
        v.sort_unstable();
        Self(v.iter().map(|d| d.as_secs_f64() * 1e6).collect())
    }

    fn at(&self, q: f64) -> f64 {
        let last = self.0.len() - 1;
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        let i = ((last as f64) * q).round() as usize;
        self.0[i]
    }

    fn p50(&self) -> f64 {
        self.at(0.50)
    }

    fn p99(&self) -> f64 {
        self.at(0.99)
    }
}

/// One store's numbers at one object count.
struct Run {
    hit: Series,
    may_hold: Series,
}

/// Times `HITS` hits round `entries`, and separately the `may_hold` loop a hit runs
/// over each entry's objects. Every hit must be served (`Some`) and every `may_hold`
/// answer yes: a miss here would be timed as if it were a hit.
async fn time<M: MetaLog, O: ObjectStore>(cache: &Cache<M, O>, entries: &[Entry]) -> Run {
    for e in entries {
        // Warm-up, untimed: the first read of each object.
        assert!(
            cache.action_result(&e.action).await.expect("hit").is_some(),
            "a warm-up read missed"
        );
    }
    let mut hit = Vec::with_capacity(HITS);
    for i in 0..HITS {
        let e = &entries[i % entries.len()];
        let started = Instant::now();
        let served = cache.action_result(&e.action).await.expect("hit");
        hit.push(started.elapsed());
        assert!(served.is_some(), "a timed hit was a miss");
    }
    let mut keys = Vec::with_capacity(entries.len());
    for e in entries {
        let action = e.action;
        let objects = cache
            .meta()
            .query(move |s| s.stored_objects(&action))
            .await
            .expect("query");
        assert_eq!(objects.len(), e.objects, "objects went missing");
        let k: Vec<_> = objects
            .into_iter()
            .map(|o| cache.object_key(o).expect("a key"))
            .collect();
        keys.push(k);
    }
    let mut may_hold = Vec::with_capacity(HITS);
    for i in 0..HITS {
        let k = &keys[i % keys.len()];
        let started = Instant::now();
        for key in k {
            let held = cache.objects().may_hold(key).await.expect("may_hold");
            assert!(held, "the store does not hold an object the hit needs");
        }
        may_hold.push(started.elapsed());
    }
    Run {
        hit: Series::new(hit),
        may_hold: Series::new(may_hold),
    }
}

#[allow(clippy::cast_precision_loss)]
fn count(n: usize) -> f64 {
    n as f64
}

async fn open<O: ObjectStore>(objects: O) -> Cache<MemoryMetaLog, O> {
    Cache::open(
        MemoryMetaLog::new(Retention::default()),
        objects,
        KeyPrefix::default(),
    )
    .await
    .expect("open the cache")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "a measurement, run by hand or as a non-gating CI step (see the module doc)"]
async fn measure_action_cache_hit_over_fs_and_memory_stores() {
    println!(
        "measure_hit: actions={ACTIONS} hits={HITS} output_bytes={OUTPUT_BYTES} (latencies in us)"
    );
    for (run, m) in OBJECTS_PER_HIT.into_iter().enumerate() {
        let memory = open(MemoryStore::new(Capabilities::default())).await;
        let mem_entries = entries(&memory, m, run).await;
        let mem = time(&memory, &mem_entries).await;

        let dir = scratch(&format!("m{m}"));
        let fs = open(FsStore::open(&dir, FLOOR).expect("open the file store")).await;
        let fs_entries = entries(&fs, m, run).await;
        let file = time(&fs, &fs_entries).await;
        drop(fs);
        let _ = std::fs::remove_dir_all(&dir);

        let per = |s: &Series| s.p50() / count(m);
        for (name, r) in [("memory", &mem), ("fs", &file)] {
            println!(
                "measure_hit: store={name} M={m} hit_p50={:.1} hit_p99={:.1} \
                 may_hold_p50={:.1} may_hold_p99={:.1} may_hold_us_per_object_p50={:.2} \
                 may_hold_share_of_hit_p50={:.2}",
                r.hit.p50(),
                r.hit.p99(),
                r.may_hold.p50(),
                r.may_hold.p99(),
                per(&r.may_hold),
                r.may_hold.p50() / r.hit.p50(),
            );
        }
        println!(
            "measure_hit: M={m} fs/memory hit_p50={:.2} hit_p99={:.2} \
             hit_p50_extra_us_per_object={:.2}",
            file.hit.p50() / mem.hit.p50(),
            file.hit.p99() / mem.hit.p99(),
            (file.hit.p50() - mem.hit.p50()) / count(m),
        );
    }
}
