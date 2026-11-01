//! Cell B, cold read with copy-back.
//!
//! Quantity: the time of a read through the instance of an object that is on the cold store only
//! (the first read, which the instance copies back to its hot tier), the second read of the same
//! object, and a read of the cold store directly. Per object, in this order: the first read, the
//! second, and last the direct read of the cold bucket (if the run configures the cold role). The
//! direct read is last because a read of the cold bucket before the first read would leave the
//! object in the page cache of the store's disks, and the miss would be timed against a warm
//! store; the direct read after them is the warm one, which the label says.
//!
//! Arms (`<size>-c<in flight>`): the first read's time to the last byte; the arm with the suffix
//! `.ttfb` its time to the first byte (the first byte of a miss of a 128 MiB object is the
//! quantity of the paper); `.second` the second read; `.cold-direct` the direct read. The
//! objects of a pass are written at the start of the pass, as multipart uploads through the
//! instance, which sends them to its cold tier only (it has no cold-only mode at the pinned tag
//! and runs two-tier), so no object has been read before its first read; they are deleted through
//! the instance, and from the cold bucket directly, at the end of the pass. The direct read is of
//! the same key in the cold bucket and finds it only if the instance keeps its cold objects under
//! the keys it was given; where it does not, those reads are failures and say so. The cold reads
//! are labelled "recently written, likely cache-warm on the store; a lower bound on a real miss".
//!
//! Sizes (4 KiB, 1 MiB, 128 MiB unless `--object-bytes` says), 1,000, 300 and 8 objects per pass
//! as the plan has them, requests in flight as asked.

use std::sync::Arc;

use bytes::Bytes;

use crate::cells::content;
use crate::cells::engine::{
    Arm, Cap, Charger, Env, LoadSource, Pace, ProcLoad, Step, hook, run_cell, slug, work,
};
use crate::cells::modes::{KIB, MIB, width_ok};
use crate::cells::putcell::size_label;
use crate::cells::streamget::StreamGet;
use crate::cells::{CellContext, CellId, CellPlan};
use crate::error::BenchError;
use crate::result::{CellResult, Op};
use crate::role::Role;
use crate::s3::BenchS3;

/// Sizes of the cell when `--object-bytes` names none.
pub const DEFAULT_SIZES: [u64; 3] = [4 * KIB, MIB, 128 * MIB];

/// Objects per pass: 1,000 up to 64 KiB, 300 up to 1 MiB, 8 above.
#[must_use]
pub fn objects_for(size: u64) -> u64 {
    match size {
        0..=65_536 => 1000,
        65_537..=MIB => 300,
        _ => 8,
    }
}

/// What one arm reads and writes.
struct Misses {
    instance: Arc<BenchS3>,
    cold: Option<Arc<BenchS3>>,
    read_instance: Arc<StreamGet>,
    read_cold: Option<Arc<StreamGet>>,
    charger: Charger,
    prefix: String,
    size: u64,
    objects: u64,
    body: Bytes,
}

impl Misses {
    fn key(&self, pass: u32, index: u64) -> String {
        format!("{}/p{pass}/{index}", self.prefix)
    }

    /// Writes the objects of `pass` through the instance as multipart uploads.
    async fn write(&self, pass: u32) -> Result<(), BenchError> {
        for index in 0..self.objects {
            // Counted before it is sent: an upload that is cut off may have landed.
            self.charger.charge(self.size)?;
            let put = self
                .instance
                .put_multipart(&self.key(pass, index), self.body.clone())
                .await?;
            if !put.is_ok() {
                return Err(BenchError::Setup(
                    "b: the instance would not take the objects to read".to_owned(),
                ));
            }
        }
        Ok(())
    }

    /// Deletes the objects of `pass` through the instance and from the cold bucket.
    async fn remove(&self, pass: u32) -> Result<String, BenchError> {
        let mut deleted = 0;
        for index in 0..self.objects {
            let key = self.key(pass, index);
            deleted += u64::from(self.instance.delete(&key).await?.is_ok());
            if let Some(cold) = &self.cold {
                cold.delete(&key).await?;
            }
        }
        let aborted = self
            .instance
            .abort_uploads(&format!("{}/p{pass}/", self.prefix))
            .await?;
        Ok(format!(
            "cleanup: {deleted} of {} deletes through the instance answered, {aborted} unfinished uploads aborted",
            self.objects
        ))
    }

    /// The reads of one object: the first and the second through the instance, then, last,
    /// direct from the cold bucket (see the header for why last).
    async fn read(&self, pass: u32, index: u64) -> Result<Vec<Step>, BenchError> {
        let key = self.key(pass, index);
        let mut steps = Vec::new();
        let first = self.read_instance.get(&key, None).await?;
        let full = Step::streamed(Role::Instance, Op::Get, self.size, &first);
        steps.extend(first.ttfb.map(|t| {
            Step {
                latency_us: crate::cells::engine::micros(t),
                ..full.clone()
            }
            .second_quantity_of_request(".ttfb")
        }));
        steps.push(full);
        let second = self.read_instance.get(&key, None).await?;
        steps.push(Step::streamed(Role::Instance, Op::Get, self.size, &second).labelled(".second"));
        if let Some(cold) = &self.read_cold {
            let direct = cold.get(&key, None).await?;
            steps.push(
                Step::streamed(Role::Cold, Op::Get, self.size, &direct).labelled(".cold-direct"),
            );
        }
        Ok(steps)
    }
}

/// The arms of the cell.
///
/// # Errors
/// [`BenchError`] if the instance role is not configured or a client cannot be made.
pub fn arms(env: &Env, plan: &CellPlan, limit: Option<u64>) -> Result<Vec<Arm>, BenchError> {
    let sizes: Vec<u64> = if plan.object_bytes.is_empty() {
        DEFAULT_SIZES.to_vec()
    } else {
        plan.object_bytes.clone()
    };
    let has_cold = env.config.stores.contains_key(&Role::Cold);
    let (cold, read_cold) = if has_cold {
        (
            Some(env.s3(Role::Cold, Cap::Cold)?),
            Some(env.stream(Role::Cold, Cap::Cold)?),
        )
    } else {
        (None, None)
    };
    let instance = env.s3(Role::Instance, Cap::Hot)?;
    let read_instance = env.stream(Role::Instance, Cap::Hot)?;
    let mut out = Vec::new();
    for &size in &sizes {
        for &in_flight in plan.in_flight.iter().filter(|&&n| width_ok(n, size)) {
            let objects = objects_for(size).min(limit.unwrap_or(u64::MAX));
            let label = slug(&format!("{}-c{in_flight}", size_label(size)));
            let misses = Arc::new(Misses {
                instance: instance.clone(),
                cold: cold.clone(),
                read_instance: read_instance.clone(),
                read_cold: read_cold.clone(),
                charger: env.charger(),
                prefix: format!("{}/b/{label}", env.prefix()),
                size,
                objects,
                body: content::body(env.config.seed ^ size, size),
            });
            out.push(arm(label, in_flight, &misses));
        }
    }
    Ok(out)
}

fn arm(label: String, in_flight: u32, misses: &Arc<Misses>) -> Arm {
    let (before, after, reads) = (misses.clone(), misses.clone(), misses.clone());
    let notes = vec![
        format!(
            "b: a read through the instance of an object only the cold tier holds, {} objects of {} a pass, written as multipart uploads through the instance at the start of each pass; the arms .ttfb, .second and .cold-direct are its first byte, the second read of the same object and the direct read of the cold bucket",
            misses.objects,
            size_label(misses.size)
        ),
        "recently written, likely cache-warm on the store; a lower bound on a real miss".to_owned(),
        "per object the instance's reads come first and the direct read of the cold bucket last, so the first read is not timed against a store the harness has just read from".to_owned(),
        "test instance and test buckets; two-tier instance (no cold-only mode at the pinned tag)"
            .to_owned(),
    ];
    Arm {
        label,
        pace: Pace::Closed,
        in_flight,
        requests: misses.objects,
        cold_bytes_per_pass: misses.objects * misses.size,
        cold_bytes_once: 0,
        work: work(move |unit| {
            let misses = reads.clone();
            async move { misses.read(unit.pass, unit.index).await }
        }),
        before: Some(hook(move |pass| {
            let misses = before.clone();
            async move {
                misses.write(pass).await?;
                Ok(vec![])
            }
        })),
        after: Some(hook(move |pass| {
            let misses = after.clone();
            async move { Ok(vec![misses.remove(pass).await?]) }
        })),
        faulted: false,
        notes,
        proxy: None,
    }
}

/// Runs the cell: one [`CellResult`] per arm and per second quantity of it, passes interleaved,
/// pass 0 discarded. Needs the instance role.
///
/// # Errors
/// [`BenchError`] if the instance is not configured, the objects cannot be written, the ledger
/// refuses the cell, the load gate trips or cannot be read, or a file or the request log fails.
pub async fn run(ctx: &CellContext, plan: &CellPlan) -> Result<Vec<CellResult>, BenchError> {
    run_with(ctx, plan, Arc::new(ProcLoad::new()), None).await
}

/// [`run`] with the machine given and the objects of a pass capped at `limit`.
///
/// # Errors
/// As [`run`].
pub async fn run_with(
    ctx: &CellContext,
    plan: &CellPlan,
    load: Arc<dyn LoadSource>,
    limit: Option<u64>,
) -> Result<Vec<CellResult>, BenchError> {
    let env = Env::new(ctx, load)?;
    let arms = arms(&env, plan, limit)?;
    run_cell(&env, CellId::B, arms, plan.passes, None).await
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use bytes::Bytes;
    use objstore_s3::s3::fake::FakeS3;

    use super::*;
    use crate::cells::testkit::{self, Script};
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

    /// Writes into the cold fake the objects the cell will write through the instance, as a
    /// tierstore that keeps its cold objects under their own keys would have them.
    async fn mirror(ctx: &CellContext, size: usize, passes: u32, objects: u64) {
        let env = Env::new(ctx, Script::calm()).unwrap();
        let cold = env.s3(Role::Cold, Cap::Cold).unwrap();
        for pass in 0..passes {
            for index in 0..objects {
                let key = format!("{}/b/4kib-c2/p{pass}/{index}", env.prefix());
                cold.put(&key, Bytes::from(vec![9u8; size])).await.unwrap();
            }
        }
    }

    /// Catches a count that drifted from the plan.
    #[test]
    fn the_object_counts_are_the_plans() {
        assert_eq!(
            [4096, 65_536, MIB, 128 * MIB].map(objects_for),
            [1000, 1000, 300, 8]
        );
    }

    /// Catches the quantities of the cell lost or mislabelled: the first read, its first byte,
    /// the second read and the direct read are four arms of three objects a pass; the objects are
    /// written as parts through the instance, charged to the ledger, and gone from both stores
    /// after each pass, the cold bucket's copy included.
    #[tokio::test]
    async fn each_object_is_read_twice_through_the_instance_then_directly_and_removed() {
        let dir = temp("tb-b");
        let (instance, cold) = (FakeS3::start().await, FakeS3::start().await);
        let stores = [
            (Role::Instance, instance.endpoint()),
            (Role::Cold, cold.endpoint()),
        ];
        let ctx = testkit::ctx(dir.path(), &stores, Window::Busy);
        mirror(&ctx, 4096, 2, 3).await;
        let results = run_with(&ctx, &plan(&[2], &[4096]), Script::calm(), Some(3))
            .await
            .unwrap();
        let labels: Vec<&str> = results.iter().map(|r| r.arm.as_str()).collect();
        assert_eq!(
            labels,
            [
                "4kib-c2",
                "4kib-c2.cold-direct",
                "4kib-c2.second",
                "4kib-c2.ttfb"
            ]
        );
        for result in &results {
            assert_eq!(result.cell, CellId::B);
            assert!(
                result
                    .passes
                    .iter()
                    .all(|p| (p.requests, p.failures) == (3, 0)),
                "{}",
                result.arm
            );
        }
        let stats = instance.stats();
        assert_eq!((stats.single_puts, stats.parts, stats.completed), (0, 6, 6));
        assert!(instance.keys().is_empty());
        assert!(cold.keys().is_empty());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("ledger.json")).unwrap(),
            "{\"cold_written_bytes\":24576}"
        );
        let roles: std::collections::BTreeSet<String> = testkit::requests(dir.path())
            .iter()
            .filter(|r| (r.method.as_str(), r.bytes) == ("GET", 4096))
            .map(|r| r.role.to_string())
            .collect();
        assert_eq!(roles.into_iter().collect::<Vec<_>>(), ["cold", "instance"]);
        assert!(
            results[0]
                .notes
                .iter()
                .any(|n| n.contains("a lower bound on a real miss"))
        );
    }

    /// Catches the store being warmed by the harness before the read the paper calls a miss: for
    /// every object the first read of the key is the instance's, then its second read, and the
    /// direct read of the cold bucket comes last. A direct read first would leave the object in
    /// the page cache of the cold store's disks, and the miss would be timed against a warm store.
    #[tokio::test]
    async fn the_first_read_of_an_object_is_the_instances_and_the_direct_read_is_last() {
        let dir = temp("tb-b-order");
        let (instance, cold) = (FakeS3::start().await, FakeS3::start().await);
        let stores = [
            (Role::Instance, instance.endpoint()),
            (Role::Cold, cold.endpoint()),
        ];
        let ctx = testkit::ctx(dir.path(), &stores, Window::Busy);
        mirror(&ctx, 4096, 2, 3).await;
        run_with(&ctx, &plan(&[2], &[4096]), Script::calm(), Some(3))
            .await
            .unwrap();
        let mut reads: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for record in testkit::requests(dir.path())
            .into_iter()
            .filter(|r| r.method == "GET" && !r.key.ends_with('/'))
        {
            reads
                .entry(record.key.clone())
                .or_default()
                .push(record.role.to_string());
        }
        assert_eq!(reads.len(), 6, "{reads:?}");
        for (key, roles) in &reads {
            assert_eq!(roles, &["instance", "instance", "cold"], "{key}");
        }
    }

    /// Catches a direct read that is not a failure when the instance keeps its cold objects
    /// under other keys (the cold bucket has none of ours), and a run without the cold role
    /// still having a direct arm.
    #[tokio::test]
    async fn a_cold_bucket_without_the_keys_makes_the_direct_reads_fail_and_no_cold_role_no_arm() {
        let dir = temp("tb-b-nokeys");
        let (instance, cold) = (FakeS3::start().await, FakeS3::start().await);
        let stores = [
            (Role::Instance, instance.endpoint()),
            (Role::Cold, cold.endpoint()),
        ];
        let ctx = testkit::ctx(dir.path(), &stores, Window::Busy);
        let results = run_with(&ctx, &plan(&[1], &[4096]), Script::calm(), Some(2))
            .await
            .unwrap();
        let direct = results
            .iter()
            .find(|r| r.arm.ends_with(".cold-direct"))
            .unwrap();
        assert!(
            direct
                .passes
                .iter()
                .all(|p| (p.requests, p.failures) == (2, 2))
        );
        let others = results.iter().filter(|r| !r.arm.ends_with(".cold-direct"));
        assert!(others.flat_map(|r| &r.passes).all(|p| p.failures == 0));

        let dir = temp("tb-b-nocold");
        let ctx = testkit::ctx(
            dir.path(),
            &[(Role::Instance, instance.endpoint())],
            Window::Busy,
        );
        let results = run_with(&ctx, &plan(&[1], &[4096]), Script::calm(), Some(2))
            .await
            .unwrap();
        let labels: Vec<&str> = results.iter().map(|r| r.arm.as_str()).collect();
        assert_eq!(labels, ["4kib-c1", "4kib-c1.second", "4kib-c1.ttfb"]);
    }

    /// Catches 64 requests in flight being used on objects of more than 1 MiB, and a run without
    /// the instance going on: the arms are the plan's, and the instance is required.
    #[test]
    fn wide_arms_are_for_small_objects_only_and_the_instance_is_required() {
        let dir = temp("tb-b-arms");
        let ctx = testkit::ctx(
            dir.path(),
            &[(Role::Instance, "http://127.0.0.1:9")],
            Window::Busy,
        );
        let env = Env::new(&ctx, Script::calm()).unwrap();
        let arms = arms(&env, &plan(&[1, 64], &[4096, 2 * MIB]), None).unwrap();
        let labels: Vec<&str> = arms.iter().map(|a| a.label.as_str()).collect();
        assert_eq!(labels, ["4kib-c1", "4kib-c64", "2mib-c1"]);
        assert_eq!(arms[0].requests, 1000);
        assert_eq!(arms[2].requests, 8);
        assert_eq!(arms[2].cold_bytes_per_pass, 8 * 2 * MIB);

        let bare = testkit::ctx(dir.path(), &[], Window::Busy);
        let env = Env::new(&bare, Script::calm()).unwrap();
        assert!(matches!(arms_err(&env), BenchError::Config(_)));
    }

    fn arms_err(env: &Env) -> BenchError {
        arms(env, &plan(&[1], &[]), None).err().unwrap()
    }

    /// Catches an instance that will not take the objects being a run that read nothing.
    #[tokio::test]
    async fn an_instance_that_will_not_take_the_objects_stops_the_run() {
        let dir = temp("tb-b-dead");
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let dead = format!("http://{}", closed.local_addr().unwrap());
        drop(closed);
        let ctx = testkit::ctx(dir.path(), &[(Role::Instance, &dead)], Window::Busy);
        let err = run_with(&ctx, &plan(&[1], &[4096]), Script::calm(), Some(2))
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("instance would not take the objects"),
            "{err}"
        );
    }

    /// Catches a request of the cell whose record cannot be written being taken for a good one:
    /// the log refuses from its n-th write, for every n, until a run is long enough not to meet
    /// it.
    #[tokio::test]
    async fn a_log_that_fails_at_any_request_stops_the_run() {
        let mut finished = false;
        for taken in 0..400 {
            let dir = temp("tb-b-log");
            let (instance, cold) = (FakeS3::start().await, FakeS3::start().await);
            let stores = [
                (Role::Instance, instance.endpoint()),
                (Role::Cold, cold.endpoint()),
            ];
            let log = Arc::new(crate::requestlog::RequestLog::to_writer(
                testkit::FailAfter(taken),
            ));
            let ctx = testkit::ctx_with_log(dir.path(), &stores, Window::Busy, log);
            match run_with(&ctx, &plan(&[1], &[4096]), Script::calm(), Some(1)).await {
                Ok(_) => {
                    finished = true;
                    break;
                }
                Err(e) => assert!(matches!(e, BenchError::Log(_)), "{taken}: {e}"),
            }
        }
        assert!(finished);
    }

    /// Catches `run` going on without the instance.
    #[tokio::test]
    async fn the_run_verb_needs_the_instance() {
        let dir = temp("tb-b-run");
        let ctx = testkit::ctx(dir.path(), &[], Window::Busy);
        assert!(run(&ctx, &plan(&[1], &[])).await.is_err());
    }
}
