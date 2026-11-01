//! Cell cold-read, reads from the cold store.
//!
//! Quantity: the time to first byte and to the last byte of a read of an object that is on the
//! cold store only, and of a ranged 4 KiB read of a 16 or 128 MiB object, with the cold store healthy,
//! slow and stopped (through the fault proxy).
//!
//! Arms, each labelled with its path: `direct` (this tool against the test cold bucket) and
//! `instance` (through the instance). The objects are written by the cell itself under a key
//! prefix of its own, before the first pass of the first arm that reads them: for `direct` one
//! PUT each; for `instance` a multipart upload, which the instance sends to the cold tier only,
//! so the hot tier does not hold them (the instance has no cold-only mode at the pinned tag: it
//! runs two-tier). The reads cycle through a few objects per size, and happen right after the
//! write, so they are labelled "recently written, likely cache-warm on the store; a lower bound
//! on a real cold read". The warm-up pass 0 settles the objects.
//!
//! Each read is one request, streamed ([`crate::cells::streamget`]): the arm's latency is
//! the time to the last byte and the arm `<label>.ttfb` has the time to the first. A read that
//! gives other than the bytes the object has is a failure. Requests in flight 1, 16, 64 as asked.
//! With a fault proxy the cell notes, per pass, how many requests went through it: for the
//! `instance` arms, the cold reads the instance made. The objects are deleted after the last
//! pass, with the proxy in pass mode, also when the run stops early.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use tokio::sync::OnceCell;

use crate::cells::content;
use crate::cells::engine::{
    Arm, Cap, Charger, Env, FaultSwitch, Hook, LoadSource, Pace, ProcLoad, Step, hook, run_cell,
    slug, work,
};
use crate::cells::modes::{KIB, MIB, Mode, read_modes, width_ok};
use crate::cells::putcell::size_label;
use crate::cells::streamget::StreamGet;
use crate::cells::{CellContext, CellId, CellPlan};
use crate::error::BenchError;
use crate::proxy::FaultMode;
use crate::result::{CellResult, Op};
use crate::role::Role;
use crate::s3::BenchS3;

/// Sizes of the cell when `--object-bytes` names none.
pub const DEFAULT_SIZES: [u64; 5] = [4 * KIB, 64 * KIB, MIB, 16 * MIB, 128 * MIB];

/// A size from which a ranged read of 4 KiB is measured too (the plan names 128 MiB; the 16 MiB
/// objects get it as well, which costs 100 reads of 4 KiB).
const RANGE_FROM: u64 = 16 * MIB;

/// The bytes of a ranged read.
const RANGE_LEN: u64 = 4 * KIB;

/// One way of reading an object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadPath {
    /// The name in the arm label.
    pub name: &'static str,
    /// The store the objects are written to and read from.
    pub role: Role,
    /// Whether the objects are written as multipart uploads (cold tier only, through the
    /// instance) or one PUT each.
    pub multipart: bool,
}

/// The paths of the cell.
pub const PATHS: [ReadPath; 2] = [
    ReadPath {
        name: "direct",
        role: Role::Cold,
        multipart: false,
    },
    ReadPath {
        name: "instance",
        role: Role::Instance,
        multipart: true,
    },
];

/// Requests per pass at `size`: 1,000 up to 64 KiB, 300 at 1 MiB, 50 at 16 MiB, 8 above.
#[must_use]
pub fn requests_for(size: u64, _in_flight: u32) -> u64 {
    match size {
        0..=65_536 => 1000,
        65_537..=MIB => 300,
        _ if size <= 16 * MIB => 50,
        _ => 8,
    }
}

/// Objects written per path and size: 100 up to 64 KiB, 50 at 1 MiB, 10 at 16 MiB, 4 above.
fn objects_for(size: u64) -> u64 {
    match size {
        0..=65_536 => 100,
        65_537..=MIB => 50,
        _ if size <= 16 * MIB => 10,
        _ => 4,
    }
}

/// The objects one path reads at one size, written once, deleted at the end.
struct ObjectSet {
    path: ReadPath,
    size: u64,
    keys: Vec<String>,
    body: Bytes,
    writer: Arc<BenchS3>,
    charger: Charger,
    ready: OnceCell<()>,
}

impl ObjectSet {
    /// Writes the objects if they are not written yet.
    async fn ensure(&self) -> Result<(), BenchError> {
        self.ready
            .get_or_try_init(|| async {
                for key in &self.keys {
                    // Counted before it is sent: a put that is cut off may have landed.
                    self.charger.charge(self.size)?;
                    let outcome = if self.path.multipart {
                        self.writer.put_multipart(key, self.body.clone()).await?
                    } else {
                        self.writer.put(key, self.body.clone()).await?
                    };
                    if !outcome.is_ok() {
                        return Err(BenchError::Setup(format!(
                            "cold-read: the store of role {} would not take the objects to read",
                            self.path.role
                        )));
                    }
                }
                Ok(())
            })
            .await
            .map(|_| ())
    }

    /// Deletes the objects; how many deletes were answered.
    async fn clean(&self) -> Result<u64, BenchError> {
        let mut deleted = 0;
        for key in &self.keys {
            deleted += u64::from(self.writer.delete(key).await?.is_ok());
        }
        Ok(deleted)
    }
}

/// What one read arm sends.
#[derive(Clone)]
struct Reader {
    set: Arc<ObjectSet>,
    stream: Arc<StreamGet>,
    /// `None` for the whole object, else the bytes of a ranged read.
    range: Option<u64>,
}

impl Reader {
    async fn read(&self, index: u64) -> Result<Vec<Step>, BenchError> {
        let keys = &self.set.keys;
        let key = &keys[usize::try_from(index % keys.len() as u64).expect("an index")];
        let (window, expected) = match self.range {
            None => (None, self.set.size),
            Some(len) => {
                let spread = (self.set.size / len).max(1);
                (Some((index * 7919 % spread * len, len)), len)
            }
        };
        let read = self.stream.get(key, window).await?;
        let full = Step::streamed(self.set.path.role, Op::Get, expected, &read);
        let first = Step {
            latency_us: read.ttfb.map_or(0, crate::cells::engine::micros),
            ..full.clone()
        }
        .second_quantity_of_request(".ttfb");
        Ok(if read.ttfb.is_some() && full.ok {
            vec![full, first]
        } else {
            vec![full]
        })
    }
}

fn notes(path: &ReadPath, mode: &Mode, size: u64, range: Option<u64>) -> Vec<String> {
    let written = if path.multipart {
        "written as multipart uploads through the instance, so that only the cold tier holds them (the instance has no cold-only mode at the pinned tag and runs two-tier)"
    } else {
        "written by one PUT each straight to the cold bucket"
    };
    let what = match range {
        Some(len) => format!(
            "ranged reads of {len} bytes of {} objects",
            size_label(size)
        ),
        None => format!("whole reads of {} objects", size_label(size)),
    };
    vec![
        format!("cold-read by {}: {what}, {written}", path.name),
        "recently written, likely cache-warm on the store; a lower bound on a real cold read"
            .to_owned(),
        format!(
            "test instance and test buckets; fault: {}; the arm of the same name with the suffix .ttfb has the time to the first byte",
            mode.fault.map_or_else(
                || "none".to_owned(),
                |f| format!("{f:?} through the fault proxy")
            )
        ),
    ]
}

/// The arms of the cell and the cleanup that deletes what they read.
///
/// # Errors
/// [`BenchError`] if none of the cell's roles is configured or a client cannot be made.
pub fn arms(
    env: &Env,
    plan: &CellPlan,
    faults: Option<Arc<dyn FaultSwitch>>,
    limit: Option<u64>,
) -> Result<(Vec<Arm>, Hook), BenchError> {
    let sizes: Vec<u64> = if plan.object_bytes.is_empty() {
        DEFAULT_SIZES.to_vec()
    } else {
        plan.object_bytes.clone()
    };
    let smallest = sizes.iter().copied().min().unwrap_or(KIB);
    let cap = limit.unwrap_or(u64::MAX);
    let mut out = Vec::new();
    let mut sets = Vec::new();
    for path in PATHS
        .iter()
        .filter(|p| env.config.stores.contains_key(&p.role))
    {
        let writer = env.s3(path.role, Cap::Cold)?;
        let stream = env.stream(path.role, Cap::Cold)?;
        for &size in &sizes {
            let n = objects_for(size).min(cap);
            let set = Arc::new(ObjectSet {
                path: *path,
                size,
                keys: (0..n)
                    .map(|i| {
                        format!(
                            "{}/read/{}-{}/{i}",
                            env.prefix(),
                            path.name,
                            size_label(size)
                        )
                    })
                    .collect(),
                body: content::body(env.config.seed ^ size, size),
                writer: writer.clone(),
                charger: env.charger(),
                ready: OnceCell::new(),
            });
            sets.push(set.clone());
            let mut first = true;
            for mode in read_modes(faults.is_some()) {
                if !mode.wants(size, smallest) {
                    continue;
                }
                let flights = mode
                    .in_flight
                    .map_or_else(|| plan.in_flight.clone(), |n| vec![n]);
                let mut variants: Vec<(u32, Option<u64>)> = flights
                    .iter()
                    .filter(|&&n| width_ok(n, size))
                    .map(|&n| (n, None))
                    .collect();
                if size >= RANGE_FROM && mode.in_flight.is_none() {
                    variants.push((1, Some(RANGE_LEN)));
                }
                for (in_flight, range) in variants {
                    let requests = match range {
                        Some(_) => 100,
                        None => mode.requests(size, in_flight, requests_for),
                    };
                    let label = slug(&format!(
                        "{}-{}{}-c{in_flight}-{}",
                        path.name,
                        size_label(size),
                        if range.is_some() { "-range4kib" } else { "" },
                        mode.name
                    ));
                    let reader = Reader {
                        set: set.clone(),
                        stream: stream.clone(),
                        range,
                    };
                    let once = if std::mem::take(&mut first) {
                        n * size
                    } else {
                        0
                    };
                    out.push(arm(
                        label,
                        in_flight,
                        requests.min(cap),
                        once,
                        notes(path, &mode, size, range),
                        reader,
                        mode.fault.zip(faults.clone()),
                    ));
                }
            }
        }
    }
    if sets.is_empty() {
        return Err(BenchError::Setup(
            "cold-read: neither the role cold nor the role instance is configured".to_owned(),
        ));
    }
    Ok((out, cleanup(sets, faults)))
}

fn arm(
    label: String,
    in_flight: u32,
    requests: u64,
    once: u64,
    notes: Vec<String>,
    reader: Reader,
    fault: Option<(FaultMode, Arc<dyn FaultSwitch>)>,
) -> Arm {
    let seen = Arc::new(AtomicU64::new(0));
    let before_set = reader.set.clone();
    let faulted = fault
        .as_ref()
        .is_some_and(|(mode, _)| *mode != FaultMode::Pass);
    let (before_fault, after_fault) = (fault.clone(), fault.map(|(_, switch)| switch));
    let (before_seen, after_seen) = (seen.clone(), seen);
    let proxy = after_fault.clone();
    Arm {
        label,
        pace: Pace::Closed,
        in_flight,
        requests,
        cold_bytes_per_pass: 0,
        cold_bytes_once: once,
        work: work(move |unit| {
            let reader = reader.clone();
            async move { reader.read(unit.index).await }
        }),
        before: Some(hook(move |_| {
            let (set, fault, seen) = (
                before_set.clone(),
                before_fault.clone(),
                before_seen.clone(),
            );
            async move {
                if let Some((_, switch)) = &fault {
                    switch.set_mode(FaultMode::Pass)?;
                }
                set.ensure().await?;
                if let Some((mode, switch)) = &fault {
                    switch.set_mode(*mode)?;
                    seen.store(switch.request_count(), Ordering::SeqCst);
                }
                Ok(vec![])
            }
        })),
        after: Some(hook(move |_| {
            let (switch, seen) = (after_fault.clone(), after_seen.clone());
            async move {
                switch.map_or_else(
                    || Ok(Vec::new()),
                    |switch| {
                        let went = switch
                            .request_count()
                            .saturating_sub(seen.load(Ordering::SeqCst));
                        switch.set_mode(FaultMode::Pass)?;
                        Ok(vec![format!(
                            "{went} requests went through the fault proxy in the pass"
                        )])
                    },
                )
            }
        })),
        faulted,
        notes,
        proxy,
    }
}

/// Deletes every object of the cell, with the proxy back in pass mode.
fn cleanup(sets: Vec<Arc<ObjectSet>>, faults: Option<Arc<dyn FaultSwitch>>) -> Hook {
    let sets = Arc::new(sets);
    hook(move |_| {
        let (sets, faults) = (sets.clone(), faults.clone());
        async move {
            if let Some(switch) = &faults {
                switch.set_mode(FaultMode::Pass)?;
            }
            let (mut deleted, mut total) = (0, 0);
            for set in sets.iter() {
                deleted += set.clean().await?;
                total += set.keys.len();
            }
            Ok(vec![format!(
                "cleanup: {deleted} of {total} objects deleted"
            )])
        }
    })
}

/// Runs the cell against the stores the run configures: one [`CellResult`] per arm (and one per
/// `.ttfb` arm), passes interleaved, pass 0 discarded.
///
/// # Errors
/// [`BenchError`] if no store of the cell is configured, the objects cannot be written, the
/// ledger refuses the cell, the load gate trips or cannot be read, or a file or the request log
/// fails.
pub async fn run(ctx: &CellContext, plan: &CellPlan) -> Result<Vec<CellResult>, BenchError> {
    run_with(ctx, plan, Arc::new(ProcLoad::new()), None, None).await
}

/// [`run`] with the machine, the fault proxy and a cap on the requests of a pass (and the
/// objects of a size) given.
///
/// # Errors
/// As [`run`].
pub async fn run_with(
    ctx: &CellContext,
    plan: &CellPlan,
    load: Arc<dyn LoadSource>,
    faults: Option<Arc<dyn FaultSwitch>>,
    limit: Option<u64>,
) -> Result<Vec<CellResult>, BenchError> {
    let env = Env::new(ctx, load)?;
    let (arms, cleanup) = arms(&env, plan, faults, limit)?;
    run_cell(&env, CellId::ColdRead, arms, plan.passes, Some(cleanup)).await
}

#[cfg(test)]
mod tests {
    use objstore_s3::s3::fake::FakeS3;

    use super::*;
    use crate::cells::testkit::{self, Script, SwitchLog};
    use crate::result::Window;

    fn plan(in_flight: &[u32], sizes: &[u64]) -> CellPlan {
        CellPlan {
            passes: 2,
            in_flight: in_flight.to_vec(),
            object_bytes: sizes.to_vec(),
            open_loop: true,
        }
    }

    fn temp(name: &str) -> kbf_testkit::dir::TestDir {
        kbf_testkit::dir::TestDir::under(std::env::temp_dir(), name)
    }

    /// Catches counts and object numbers that drifted from the plan.
    #[test]
    fn the_counts_by_size_are_the_plans() {
        let counts = |size| (requests_for(size, 1), objects_for(size));
        assert_eq!(counts(4096), (1000, 100));
        assert_eq!(counts(64 * KIB), (1000, 100));
        assert_eq!(counts(MIB), (300, 50));
        assert_eq!(counts(16 * MIB), (50, 10));
        assert_eq!(counts(128 * MIB), (8, 4));
    }

    /// Catches objects written the wrong way for their path (a plain put where the instance
    /// must get parts), reads that do not reach the store they name, a time to first byte that
    /// is missing, objects left behind, and objects not charged to the ledger: two paths, two
    /// passes, three objects each.
    #[tokio::test]
    async fn the_objects_are_written_once_read_streamed_and_deleted() {
        let dir = temp("tb-cold-read");
        let (cold, instance) = (FakeS3::start().await, FakeS3::start().await);
        let stores = [
            (Role::Cold, cold.endpoint()),
            (Role::Instance, instance.endpoint()),
        ];
        let ctx = testkit::ctx(dir.path(), &stores, Window::Busy);
        let results = run_with(&ctx, &plan(&[2], &[4096]), Script::calm(), None, Some(3))
            .await
            .unwrap();
        let labels: Vec<&str> = results.iter().map(|r| r.arm.as_str()).collect();
        assert_eq!(
            labels,
            [
                "direct-4kib-c2-healthy",
                "direct-4kib-c2-healthy.ttfb",
                "instance-4kib-c2-healthy",
                "instance-4kib-c2-healthy.ttfb"
            ]
        );
        for result in &results {
            assert_eq!(result.cell, CellId::ColdRead);
            assert!(
                result
                    .passes
                    .iter()
                    .all(|p| (p.requests, p.failures) == (3, 0))
            );
        }
        let (c, i) = (cold.stats(), instance.stats());
        assert_eq!((c.single_puts, c.parts), (3, 0));
        assert_eq!((i.single_puts, i.parts, i.completed), (0, 3, 3));
        assert!(cold.keys().is_empty());
        assert!(instance.keys().is_empty());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("ledger.json")).unwrap(),
            "{\"cold_written_bytes\":24576}"
        );
        assert!(
            results[0]
                .notes
                .iter()
                .any(|n| n == "cleanup: 6 of 6 objects deleted")
        );
        assert!(
            results[0]
                .notes
                .iter()
                .any(|n| n.contains("a lower bound on a real cold read"))
        );
        let reads = testkit::requests(dir.path())
            .into_iter()
            .filter(|r| (r.method.as_str(), r.bytes) == ("GET", 4096))
            .count();
        assert_eq!(reads, 12);
    }

    /// Catches the ranged read arm missing, reading the whole object, or reading outside it:
    /// objects of 16 MiB get a 4 KiB ranged arm whose reads give 4 KiB each.
    #[tokio::test]
    async fn a_large_object_also_gets_a_ranged_read_arm() {
        let dir = temp("tb-cold-read-range");
        let cold = FakeS3::start().await;
        let ctx = testkit::ctx(dir.path(), &[(Role::Cold, cold.endpoint())], Window::Busy);
        let results = run_with(
            &ctx,
            &plan(&[1], &[16 * MIB]),
            Script::calm(),
            None,
            Some(2),
        )
        .await
        .unwrap();
        let labels: Vec<&str> = results.iter().map(|r| r.arm.as_str()).collect();
        assert_eq!(
            labels,
            [
                "direct-16mib-c1-healthy",
                "direct-16mib-c1-healthy.ttfb",
                "direct-16mib-range4kib-c1-healthy",
                "direct-16mib-range4kib-c1-healthy.ttfb"
            ]
        );
        assert!(
            results
                .iter()
                .all(|r| r.passes.iter().all(|p| (p.failures, p.requests) == (0, 2)))
        );
        assert_eq!(results[2].passes[1].bytes, 2 * RANGE_LEN);
        assert_eq!(results[0].passes[1].bytes, 2 * 16 * MIB);
    }

    /// Catches fault modes not set (the preload must happen in pass mode, the reads in the
    /// mode), the proxy's count not noted, and a cleanup that leaves the proxy faulted: ten arms
    /// of two paths, five noted per pass.
    #[tokio::test]
    async fn faulted_arms_note_what_went_through_the_proxy_and_the_cleanup_restores_it() {
        let dir = temp("tb-cold-read-faults");
        let (cold, instance) = (FakeS3::start().await, FakeS3::start().await);
        let stores = [
            (Role::Cold, cold.endpoint()),
            (Role::Instance, instance.endpoint()),
        ];
        let ctx = testkit::ctx(dir.path(), &stores, Window::Busy);
        let switch = Arc::new(SwitchLog::default());
        let results = run_with(
            &ctx,
            &plan(&[1], &[4096, MIB]),
            Script::calm(),
            Some(switch.clone()),
            Some(2),
        )
        .await
        .unwrap();
        assert_eq!(results.len(), 24);
        let note = "5 requests went through the fault proxy in the pass";
        let noted = results
            .iter()
            .filter(|r| r.notes.iter().any(|n| n.ends_with(note)))
            .count();
        assert_eq!(noted, 24);
        let modes = switch.modes();
        assert_eq!(modes.last(), Some(&FaultMode::Pass));
        for mode in [
            FaultMode::Latency { ms: 50 },
            FaultMode::Latency { ms: 500 },
            FaultMode::Refuse,
            FaultMode::Blackhole,
        ] {
            assert_eq!(modes.iter().filter(|m| **m == mode).count(), 4, "{mode:?}");
        }
        assert!(cold.keys().is_empty());
        assert!(instance.keys().is_empty());
        assert!(
            results
                .iter()
                .any(|r| r.arm == "instance-4kib-c6-blackhole")
        );
        assert!(
            results
                .iter()
                .any(|r| r.arm == "instance-1mib-c1-latency-500ms")
        );
        assert!(
            results
                .iter()
                .all(|r| r.arm != "instance-4kib-c1-latency-50ms")
        );
    }

    /// Catches the rules of which arms exist drifting: 64 in flight not for 16 MiB objects, the
    /// ranged arm made once per path for the large ones (and not for a fault that fixes its own
    /// width), the modes of a fault proxy at the sizes they name.
    #[test]
    fn wide_arms_are_for_small_objects_and_the_ranged_arm_for_large_ones() {
        let dir = temp("tb-cold-read-arms");
        let stores = [
            (Role::Cold, "http://127.0.0.1:9"),
            (Role::Instance, "http://127.0.0.1:9"),
        ];
        let ctx = testkit::ctx(dir.path(), &stores, Window::Busy);
        let env = Env::new(&ctx, Script::calm()).unwrap();
        let labels = |arms: Vec<Arm>| {
            let mut labels: Vec<String> = arms.into_iter().map(|a| a.label).collect();
            labels.sort();
            labels
        };
        let (arms, _) = arms(&env, &plan(&[1, 64], &[4096, 16 * MIB]), None, None).unwrap();
        let direct: Vec<String> = labels(arms)
            .into_iter()
            .filter(|l| l.starts_with("direct"))
            .collect();
        assert_eq!(
            direct,
            [
                "direct-16mib-c1-healthy",
                "direct-16mib-range4kib-c1-healthy",
                "direct-4kib-c1-healthy",
                "direct-4kib-c64-healthy"
            ]
        );
        let switch: Arc<dyn FaultSwitch> = Arc::new(SwitchLog::default());
        let (arms, _) = self::arms(&env, &plan(&[1], &[16 * MIB]), Some(switch), None).unwrap();
        let direct: Vec<String> = labels(arms)
            .into_iter()
            .filter(|l| l.starts_with("direct"))
            .collect();
        assert_eq!(
            direct,
            [
                "direct-16mib-c1-proxy-pass",
                "direct-16mib-c6-blackhole",
                "direct-16mib-c6-refused",
                "direct-16mib-range4kib-c1-proxy-pass"
            ]
        );
    }

    /// Catches a store that will not take the objects being treated as a run that read
    /// nothing: the run stops with the setup named, and the cleanup still runs.
    #[tokio::test]
    async fn a_store_that_will_not_take_the_objects_stops_the_run() {
        let dir = temp("tb-cold-read-dead");
        let dead_store = testkit::DeadStore::start().await;
        let dead = dead_store.endpoint();
        let ctx = testkit::ctx(dir.path(), &[(Role::Cold, dead)], Window::Busy);
        let err = run_with(&ctx, &plan(&[1], &[4096]), Script::calm(), None, Some(2))
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("store of role cold would not take the objects"),
            "{err}"
        );
        let deletes = testkit::requests(dir.path())
            .iter()
            .filter(|r| r.method == "DELETE")
            .count();
        assert_eq!(deletes, 2);
    }

    /// Catches a read that is counted good when it is not: a key that is not there, and an object
    /// of other than the expected length, are failures; neither has a time to first byte arm
    /// row unless a byte came.
    #[tokio::test]
    async fn a_missing_or_short_object_is_a_failed_read() {
        let dir = temp("tb-cold-read-bad");
        let cold = FakeS3::start().await;
        let ctx = testkit::ctx(dir.path(), &[(Role::Cold, cold.endpoint())], Window::Busy);
        let env = Env::new(&ctx, Script::calm()).unwrap();
        let writer = env.s3(Role::Cold, Cap::Cold).unwrap();
        writer
            .put("k/short", Bytes::from(vec![1u8; 100]))
            .await
            .unwrap();
        let set = |key: &str| {
            Arc::new(ObjectSet {
                path: PATHS[0],
                size: 4096,
                keys: vec![key.to_owned()],
                body: Bytes::new(),
                writer: writer.clone(),
                charger: env.charger(),
                ready: OnceCell::new(),
            })
        };
        let reader = |key: &str| Reader {
            set: set(key),
            stream: env.stream(Role::Cold, Cap::Cold).unwrap(),
            range: None,
        };
        let missing = reader("k/none").read(0).await.unwrap();
        assert_eq!(missing.len(), 1);
        assert!(!missing[0].ok);
        assert_eq!(missing[0].status, Some(404));
        let short = reader("k/short").read(0).await.unwrap();
        assert_eq!(short.len(), 1);
        assert!(!short[0].ok);
        assert_eq!((short[0].bytes, short[0].status), (100, Some(200)));
    }

    /// Catches `run` going on without a store of the cell.
    #[tokio::test]
    async fn the_run_verb_refuses_a_run_without_the_stores() {
        let dir = temp("tb-cold-read-none");
        let ctx = testkit::ctx(
            dir.path(),
            &[(Role::Hot, "http://127.0.0.1:9")],
            Window::Busy,
        );
        let err = run(&ctx, &plan(&[1], &[])).await.unwrap_err();
        assert!(matches!(err, BenchError::Setup(_)), "{err}");
    }

    /// Catches a request of the cell whose record cannot be written being taken for a good one
    /// (a write of an object, a streamed read, a delete): the log refuses from its n-th write,
    /// for every n, until a run is long enough not to meet it.
    #[tokio::test]
    async fn a_log_that_fails_at_any_request_stops_the_run() {
        let mut finished = false;
        for taken in 0..400 {
            let dir = temp("tb-cold-read-log");
            let (cold, instance) = (FakeS3::start().await, FakeS3::start().await);
            let stores = [
                (Role::Cold, cold.endpoint()),
                (Role::Instance, instance.endpoint()),
            ];
            let log = Arc::new(crate::requestlog::RequestLog::to_writer(
                testkit::FailAfter(taken),
            ));
            let ctx = testkit::ctx_with_log(dir.path(), &stores, Window::Busy, log);
            match run_with(&ctx, &plan(&[1], &[4096]), Script::calm(), None, Some(1)).await {
                Ok(_) => {
                    finished = true;
                    break;
                }
                Err(e) => assert!(matches!(e, BenchError::Log(_)), "{taken}: {e}"),
            }
        }
        assert!(finished);
    }
}
