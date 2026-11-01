//! Cell C, copy lag.
//!
//! Quantity: from the acknowledgement of a put to the instance until the object is on the cold
//! store, as a distribution, with the time to drain a burst and the backlog as it fell.
//!
//! A burst is `N` plain puts through the instance (acknowledged from its hot tier, copied to its
//! cold tier afterwards), `N` and the size from the plan (1,000 up to 1 MiB, 8 above), sent with
//! the requests in flight asked for. Then the cold test bucket is listed under the burst's prefix,
//! once per poll, one prefix LIST (pages of up to 1,000 keys) and never a HEAD per key; an object
//! is on the cold store at the first poll that lists it, so the lag has the resolution of the
//! poll: one over `--list-poll-rps` seconds (the cap on LISTs per second of a poll, 2 unless
//! set, so half a second). The load gate is read before the
//! burst, not during it.
//!
//! Each object is a request of the arm (`<N>x<size>-c<in flight>-<mode>`): its latency is its lag
//! in microseconds, and an object that is not listed by the deadline is a failure whose latency
//! is the time it waited. The notes give, per pass, the time to drain (from the first
//! acknowledgement to the last object listed) and the backlog (acknowledged and not yet listed)
//! at each poll. Mode `healthy` leaves the cold store as it is; with a fault proxy, mode
//! `stopped-restored` refuses the cold store while the burst is sent and restores it before the
//! first poll, so the lag is the recovery.
//!
//! Needs the instance and the cold roles. Writes under `tierstore-bench/<run id>/c/`; after each
//! pass deletes the burst's keys through the instance and whatever is under the prefix of the cold
//! bucket.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt as _;
use tokio::time::Instant;

use crate::cells::content;
use crate::cells::engine::{
    Arm, Cap, Env, FaultSwitch, LoadSource, Pace, ProcLoad, Step, hook, micros, run_cell, slug,
    work,
};
use crate::cells::modes::{MIB, width_ok};
use crate::cells::putcell::size_label;
use crate::cells::{CellContext, CellId, CellPlan};
use crate::error::BenchError;
use crate::proxy::FaultMode;
use crate::result::CellResult;
use crate::result::Op;
use crate::role::Role;
use crate::s3::{BenchS3, unix_ms};

/// Sizes of the cell when `--object-bytes` names none.
pub const DEFAULT_SIZES: [u64; 2] = [MIB, 128 * MIB];

/// How the cold store is listed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Polling {
    /// Between two LISTs.
    pub every: Duration,
    /// After this long from the last acknowledgement an object not listed is a failure.
    pub deadline: Duration,
    /// Keys asked for in a page of a LIST (1,000 at most).
    pub page: u16,
}

/// Objects in a burst: 1,000 up to 1 MiB, 8 above.
#[must_use]
pub fn burst_for(size: u64) -> u64 {
    if size <= MIB { 1000 } else { 8 }
}

/// The poll of a run: one LIST every `1 / list_poll_rps` seconds (`--list-poll-rps`, the cap on
/// prefix LISTs per second of a copy-lag poll, taken as at least 1), pages of 1,000 keys, ten
/// minutes to drain.
#[must_use]
pub fn polling(list_poll_rps: u32) -> Polling {
    Polling {
        every: Duration::from_secs(1) / list_poll_rps.max(1),
        deadline: Duration::from_secs(600),
        page: 1000,
    }
}

/// What a watch of a burst found: the lag of each put that was seen, the backlog at each poll,
/// and when the last put was seen if all were.
type Watched = (Vec<Option<Duration>>, Vec<usize>, Option<Instant>);

/// Polls `listed` (the keys of the burst's prefix on the cold store) once every `polling.every`
/// until every acknowledged put in `acked` is in it or `polling.deadline` has passed since the last
/// acknowledgement. The lag of a put is the time from its acknowledgement to the first poll that
/// lists it, so it has the resolution of the poll.
async fn watch_with<F, Fut>(
    polling: Polling,
    acked: &[(String, Instant, bool)],
    mut listed: F,
) -> Result<Watched, BenchError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<HashSet<String>, BenchError>>,
{
    let last = acked.iter().map(|(_, at, _)| *at).max();
    let mut lag: Vec<Option<Duration>> = vec![None; acked.len()];
    let mut backlog = Vec::new();
    let drained = loop {
        let seen = listed().await?;
        let now = Instant::now();
        for (slot, (key, at, ok)) in lag.iter_mut().zip(acked) {
            if slot.is_none() && *ok && seen.contains(key) {
                *slot = Some(now.saturating_duration_since(*at));
            }
        }
        let waiting = lag
            .iter()
            .zip(acked)
            .filter(|(l, (_, _, ok))| l.is_none() && *ok)
            .count();
        backlog.push(waiting);
        if waiting == 0 {
            break Some(now);
        }
        if last.is_some_and(|l| now.saturating_duration_since(l) >= polling.deadline) {
            break None;
        }
        tokio::time::sleep(polling.every).await;
    };
    Ok((lag, backlog, drained))
}

/// What one arm sends and watches.
struct Burst {
    instance: Arc<BenchS3>,
    cold: Arc<BenchS3>,
    fault: Option<Arc<dyn FaultSwitch>>,
    prefix: String,
    size: u64,
    count: u64,
    in_flight: u32,
    body: Bytes,
    polling: Polling,
    note: Mutex<Vec<String>>,
}

impl Burst {
    fn key(&self, pass: u32, index: u64) -> String {
        format!("{}/p{pass}/{index}", self.prefix)
    }

    /// Every key under `prefix` of the cold bucket, over all pages.
    async fn listed(&self, prefix: &str) -> Result<Option<Vec<String>>, BenchError> {
        let mut keys = Vec::new();
        let mut after = None;
        loop {
            let page = self
                .cold
                .list_page(prefix, after.as_ref(), self.polling.page)
                .await?;
            let Ok(page) = page.result else {
                return Ok(None);
            };
            keys.extend(page.objects.iter().map(|o| o.key.as_str().to_owned()));
            match page.next {
                Some(next) => after = Some(next),
                None => return Ok(Some(keys)),
            }
        }
    }

    /// Sends the burst of `pass`: the key, the time of the acknowledgement and whether it was a
    /// success, for each put, in the order they were acknowledged.
    async fn send(&self, pass: u32) -> Result<Vec<(String, Instant, bool)>, BenchError> {
        let puts = futures::stream::iter(0..self.count)
            .map(|index| async move {
                let key = self.key(pass, index);
                let put = self.instance.put(&key, self.body.clone()).await?;
                Ok::<_, BenchError>((key, Instant::now(), put.is_ok()))
            })
            .buffer_unordered(self.in_flight.max(1) as usize)
            .collect::<Vec<_>>()
            .await;
        puts.into_iter().collect()
    }

    /// Lists the cold bucket until every acknowledged put is there or the deadline passes: the
    /// lag of each put that was seen, the backlog at each poll, and when the last was seen if all
    /// were.
    async fn watch(
        &self,
        pass: u32,
        acked: &[(String, Instant, bool)],
    ) -> Result<Watched, BenchError> {
        let prefix = format!("{}/p{pass}/", self.prefix);
        let (this, prefix) = (self, prefix.as_str());
        watch_with(self.polling, acked, || async move {
            let keys = this.listed(prefix).await?.unwrap_or_default();
            Ok(keys.into_iter().collect())
        })
        .await
    }

    /// Sends the burst of `pass` and watches it drain; one step per object.
    async fn run(&self, pass: u32) -> Result<Vec<Step>, BenchError> {
        let acked = self.send(pass).await?;
        if let Some(switch) = &self.fault {
            switch.set_mode(FaultMode::Pass)?;
        }
        let started_unix_ms = unix_ms();
        let (lag, backlog, drained) = self.watch(pass, &acked).await?;
        let first = acked.iter().map(|(_, at, _)| *at).min();
        let drain_us = drained.zip(first).map_or(0, |(end, start)| {
            micros(end.saturating_duration_since(start))
        });
        let landed = lag.iter().filter(|l| l.is_some()).count();
        self.note.lock().unwrap_or_else(PoisonError::into_inner).push(format!(
            "burst of {} objects of {}: {landed} on the cold store at the end, time to drain {drain_us} us from the first acknowledgement (0 if it did not drain), backlog at each poll {backlog:?}",
            self.count,
            size_label(self.size),
        ));
        let end = Instant::now();
        Ok(acked
            .iter()
            .zip(&lag)
            .map(|((_, at, ok), lag)| Step {
                label: "",
                second_quantity: false,
                role: Role::Cold,
                op: Op::Put,
                bytes: self.size,
                started_unix_ms,
                latency_us: micros(lag.unwrap_or_else(|| end.saturating_duration_since(*at))),
                status: lag.map(|_| 200),
                ok: *ok && lag.is_some(),
                cold_write: self.size,
                in_flight: 0,
            })
            .collect())
    }

    /// Deletes the burst's keys through the instance and whatever the cold bucket holds under
    /// the pass's prefix.
    async fn remove(&self, pass: u32) -> Result<String, BenchError> {
        for index in 0..self.count {
            self.instance.delete(&self.key(pass, index)).await?;
        }
        let prefix = format!("{}/p{pass}/", self.prefix);
        let left = self.listed(&prefix).await?.unwrap_or_default();
        for key in &left {
            self.cold.delete(key).await?;
        }
        Ok(format!(
            "cleanup: {} deletes through the instance, {} objects deleted from the cold bucket",
            self.count,
            left.len()
        ))
    }
}

/// The arms of the cell.
///
/// # Errors
/// [`BenchError`] if the instance or the cold role is not configured, or a client cannot be made.
pub fn arms(
    env: &Env,
    plan: &CellPlan,
    faults: Option<Arc<dyn FaultSwitch>>,
    polling: Polling,
    limit: Option<u64>,
) -> Result<Vec<Arm>, BenchError> {
    let sizes: Vec<u64> = if plan.object_bytes.is_empty() {
        DEFAULT_SIZES.to_vec()
    } else {
        plan.object_bytes.clone()
    };
    let instance = env.s3(Role::Instance, Cap::Hot)?;
    let cold = env.s3(Role::Cold, Cap::Cold)?;
    let modes = [("healthy", None)]
        .into_iter()
        .chain(faults.map(|switch| ("stopped-restored", Some(switch))));
    let mut out = Vec::new();
    for (mode, fault) in modes {
        for &size in &sizes {
            for &in_flight in plan.in_flight.iter().filter(|&&n| width_ok(n, size)) {
                let count = burst_for(size).min(limit.unwrap_or(u64::MAX));
                let label = slug(&format!("{count}x{}-c{in_flight}-{mode}", size_label(size)));
                let burst = Arc::new(Burst {
                    instance: instance.clone(),
                    cold: cold.clone(),
                    fault: fault.clone(),
                    prefix: format!("{}/c/{label}", env.prefix()),
                    size,
                    count,
                    in_flight,
                    body: content::body(env.config.seed ^ size, size),
                    polling,
                    note: Mutex::new(Vec::new()),
                });
                out.push(arm(label, mode, &burst));
            }
        }
    }
    Ok(out)
}

fn arm(label: String, mode: &str, burst: &Arc<Burst>) -> Arm {
    let (run, before, after) = (burst.clone(), burst.clone(), burst.clone());
    let notes = vec![
        format!(
            "c: copy lag of {} plain puts of {} through the instance (acknowledged from its hot tier), seen by one prefix LIST of the cold test bucket every {:?}; each object is a request, its latency its lag, the resolution that of the poll",
            burst.count,
            size_label(burst.size),
            burst.polling.every
        ),
        format!("test instance and test buckets; mode {mode}"),
        "the load gate is read before the burst, not during it".to_owned(),
    ];
    Arm {
        label,
        pace: Pace::Closed,
        in_flight: 1,
        requests: 1,
        cold_bytes_per_pass: burst.count * burst.size,
        cold_bytes_once: 0,
        work: work(move |unit| {
            let burst = run.clone();
            async move { burst.run(unit.pass).await }
        }),
        before: Some(hook(move |_| {
            let set = before
                .fault
                .as_ref()
                .map_or(Ok(()), |switch| switch.set_mode(FaultMode::Refuse));
            std::future::ready(set.map(|()| vec![]))
        })),
        after: Some(hook(move |pass| {
            let burst = after.clone();
            async move {
                if let Some(switch) = &burst.fault {
                    switch.set_mode(FaultMode::Pass)?;
                }
                let mut notes = burst
                    .note
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .split_off(0);
                notes.push(burst.remove(pass).await?);
                Ok(notes)
            }
        })),
        faulted: burst.fault.is_some(),
        notes,
        proxy: burst.fault.clone(),
    }
}

/// Runs the cell: one [`CellResult`] per arm, passes interleaved, pass 0 discarded.
///
/// # Errors
/// [`BenchError`] if the instance or cold role is not configured, the ledger refuses the cell,
/// the load gate trips or cannot be read, or a file or the request log fails.
pub async fn run(ctx: &CellContext, plan: &CellPlan) -> Result<Vec<CellResult>, BenchError> {
    let poll = polling(ctx.config.rates.list_poll_rps);
    run_with(ctx, plan, Arc::new(ProcLoad), None, poll, None).await
}

/// [`run`] with the machine, the fault proxy, the poll and a cap on the objects of a burst given.
///
/// # Errors
/// As [`run`].
pub async fn run_with(
    ctx: &CellContext,
    plan: &CellPlan,
    load: Arc<dyn LoadSource>,
    faults: Option<Arc<dyn FaultSwitch>>,
    polling: Polling,
    limit: Option<u64>,
) -> Result<Vec<CellResult>, BenchError> {
    let env = Env::new(ctx, load)?;
    let arms = arms(&env, plan, faults, polling, limit)?;
    run_cell(&env, CellId::C, arms, plan.passes, None).await
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use objstore_s3::s3::fake::FakeS3;
    use tokio::task::JoinHandle;

    use super::*;
    use crate::cells::testkit::{self, Script, SwitchLog};
    use crate::requestlog::RequestLog;
    use crate::result::Window;
    use crate::s3::{BenchS3, Timeouts};

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

    fn quick(deadline: Duration) -> Polling {
        Polling {
            every: Duration::from_millis(5),
            deadline,
            page: 2,
        }
    }

    /// A stand-in for the instance's copier: what the instance holds appears on the cold store a
    /// moment later.
    fn copier(from: FakeS3, to: &FakeS3) -> JoinHandle<()> {
        let spec = testkit::spec(to.endpoint());
        let to = to.clone();
        tokio::spawn(async move {
            let log = Arc::new(RequestLog::to_writer(std::io::sink()));
            let target = BenchS3::new(Role::Cold, &spec, Timeouts::default(), log).unwrap();
            loop {
                for key in from.keys() {
                    if !to.keys().contains(&key) {
                        target.put(&key, Bytes::from_static(b"copy")).await.unwrap();
                    }
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
    }

    /// Puts stray objects in the cold fake under the prefixes of the burst of an arm.
    async fn strays(ctx: &CellContext, arm: &str, passes: u32) {
        let env = Env::new(ctx, Script::calm()).unwrap();
        let cold = env.s3(Role::Cold, Cap::Cold).unwrap();
        for pass in 0..passes {
            let key = format!("{}/c/{arm}/p{pass}/stray", env.prefix());
            cold.put(&key, Bytes::from_static(b"x")).await.unwrap();
        }
    }

    /// Catches the burst sizes of the plan and the poll drifting.
    #[test]
    fn the_burst_and_the_poll_are_the_plans() {
        assert_eq!(
            [4096, MIB, 16 * MIB, 128 * MIB].map(burst_for),
            [1000, 1000, 8, 8]
        );
        let poll = polling(2);
        assert_eq!(
            (poll.every, poll.deadline, poll.page),
            (Duration::from_millis(500), Duration::from_secs(600), 1000)
        );
    }

    /// Catches `--list-poll-rps` not reaching the poll (it was hard-wired to one LIST a second):
    /// on the paused clock, objects that never land are listed until a four second deadline, once
    /// a second at 1 a second (5 polls, at 0, 1, 2, 3 and 4 s) and four times a second at 4 (17
    /// polls); zero is taken as one a second.
    #[tokio::test(start_paused = true)]
    async fn the_poll_rate_follows_the_setting() {
        for (rps, polls) in [(1_u32, 5_usize), (4, 17)] {
            let poll = Polling {
                deadline: Duration::from_secs(4),
                ..polling(rps)
            };
            assert_eq!(poll.every, Duration::from_secs(1) / rps);
            let acked = vec![("k".to_owned(), Instant::now(), true)];
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let counted = calls.clone();
            let (lag, backlog, drained) = watch_with(poll, &acked, move || {
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                std::future::ready(Ok(HashSet::new()))
            })
            .await
            .unwrap();
            let listed = calls.load(std::sync::atomic::Ordering::SeqCst);
            assert_eq!((listed, backlog.len()), (polls, polls), "{rps} a second");
            assert_eq!((lag, drained), (vec![None], None));
        }
        assert_eq!(polling(0).every, Duration::from_secs(1), "zero is one");
    }

    /// Catches a lag that does not have the resolution of the poll: an object that shows in the
    /// third listing was acknowledged two poll intervals before it, whatever the rate (1 s at 2 a
    /// second, 0.5 s at 4), and a put that failed is never waited for.
    #[tokio::test(start_paused = true)]
    async fn the_lag_of_an_object_is_the_time_to_the_poll_that_lists_it() {
        for rps in [2_u32, 4] {
            let acked = vec![
                ("seen".to_owned(), Instant::now(), true),
                ("failed".to_owned(), Instant::now(), false),
            ];
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let counted = calls.clone();
            let (lag, backlog, drained) = watch_with(polling(rps), &acked, move || {
                let n = counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let keys = if n >= 2 {
                    vec!["seen".to_owned()]
                } else {
                    vec![]
                };
                std::future::ready(Ok(keys.into_iter().collect()))
            })
            .await
            .unwrap();
            let two_polls = Duration::from_secs(1) / rps * 2;
            // The timer wheel rounds a sleep up to the millisecond, so allow that and no more.
            let seen = lag[0].unwrap();
            let allowed = two_polls..two_polls + Duration::from_millis(5);
            assert!(allowed.contains(&seen), "{rps} a second: {seen:?}");
            assert_eq!(lag[1], None);
            assert_eq!(backlog, [1, 1, 0]);
            assert!(drained.is_some());
        }
    }

    /// Catches a lag that is not measured from the acknowledgement to a LIST that shows the
    /// object, an object lost between the pages of the listing, a drain that is not noted, and
    /// the objects left on the instance: four puts a pass, listed two keys a page, copied a
    /// moment after.
    #[tokio::test]
    async fn the_lag_of_each_object_is_found_by_listing_the_cold_bucket() {
        let dir = temp("tb-c");
        let (instance, cold) = (FakeS3::start().await, FakeS3::start().await);
        let stores = [
            (Role::Instance, instance.endpoint()),
            (Role::Cold, cold.endpoint()),
        ];
        let ctx = testkit::ctx(dir.path(), &stores, Window::Busy);
        let copying = copier(instance.clone(), &cold);
        let results = run_with(
            &ctx,
            &plan(&[2], &[4096]),
            Script::calm(),
            None,
            quick(Duration::from_secs(60)),
            Some(4),
        )
        .await
        .unwrap();
        copying.abort();
        assert_eq!(results.len(), 1);
        let result = &results[0];
        assert_eq!(
            (result.cell, result.arm.as_str()),
            (CellId::C, "4x4kib-c2-healthy")
        );
        for pass in &result.passes {
            assert_eq!((pass.requests, pass.failures), (4, 0));
            assert!(pass.percentiles.is_some());
        }
        let noted = result
            .notes
            .iter()
            .filter(|n| n.contains("time to drain"))
            .count();
        assert_eq!(noted, 2);
        assert!(
            result
                .notes
                .iter()
                .any(|n| n.contains("backlog at each poll ["))
        );
        assert!(instance.keys().is_empty());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("ledger.json")).unwrap(),
            "{\"cold_written_bytes\":32768}"
        );
        let lists = testkit::requests(dir.path())
            .iter()
            .filter(|r| (r.method.as_str(), r.role) == ("GET", Role::Cold))
            .count();
        assert!(lists >= 4, "every poll is two pages at least: {lists}");
    }

    /// Catches objects that never reach the cold store being counted as copied: after the
    /// deadline they are failures with no status, the burst is cleaned from both stores, and the
    /// stray the cold bucket held under the prefix goes too.
    #[tokio::test]
    async fn an_object_not_listed_by_the_deadline_is_a_failure_and_the_cleanup_clears_the_cold_bucket()
     {
        let dir = temp("tb-c-never");
        let (instance, cold) = (FakeS3::start().await, FakeS3::start().await);
        let stores = [
            (Role::Instance, instance.endpoint()),
            (Role::Cold, cold.endpoint()),
        ];
        let ctx = testkit::ctx(dir.path(), &stores, Window::Quiet);
        strays(&ctx, "3x4kib-c1-healthy", 2).await;
        let results = run_with(
            &ctx,
            &plan(&[1], &[4096]),
            Script::calm(),
            None,
            quick(Duration::ZERO),
            Some(3),
        )
        .await
        .unwrap();
        let result = &results[0];
        assert!(
            result
                .passes
                .iter()
                .all(|p| (p.requests, p.failures, p.percentiles) == (3, 3, None))
        );
        assert!(
            result
                .notes
                .iter()
                .any(|n| n.contains("0 on the cold store at the end"))
        );
        assert!(
            result
                .notes
                .iter()
                .any(|n| n.ends_with("1 objects deleted from the cold bucket"))
        );
        assert!(instance.keys().is_empty());
        assert!(cold.keys().is_empty());
        let samples = std::fs::read_to_string(dir.path().join("samples.csv")).unwrap();
        assert!(samples.lines().skip(1).all(|l| l.ends_with(",,1")));
    }

    /// Catches a cold store that cannot be listed ending the run in error: the objects are
    /// failures, and the cleanup of an unlistable bucket finds nothing to delete.
    #[tokio::test]
    async fn a_cold_store_that_cannot_be_listed_leaves_the_objects_unseen() {
        let dir = temp("tb-c-coldless");
        let instance = FakeS3::start().await;
        let dead_store = testkit::DeadStore::start().await;
        let dead = dead_store.endpoint();
        let stores = [(Role::Instance, instance.endpoint()), (Role::Cold, dead)];
        let ctx = testkit::ctx(dir.path(), &stores, Window::Busy);
        let results = run_with(
            &ctx,
            &plan(&[1], &[4096]),
            Script::calm(),
            None,
            quick(Duration::ZERO),
            Some(2),
        )
        .await
        .unwrap();
        assert!(results[0].passes.iter().all(|p| p.failures == 2));
        assert!(instance.keys().is_empty());
    }

    /// Catches a put that failed being waited for until the deadline, or counted copied: an
    /// instance that is not there makes every object a failure at once.
    #[tokio::test]
    async fn puts_that_failed_are_failures_and_not_waited_for() {
        let dir = temp("tb-c-noinstance");
        let cold = FakeS3::start().await;
        let dead_store = testkit::DeadStore::start().await;
        let dead = dead_store.endpoint();
        let stores = [(Role::Instance, dead), (Role::Cold, cold.endpoint())];
        let ctx = testkit::ctx(dir.path(), &stores, Window::Busy);
        let results = run_with(
            &ctx,
            &plan(&[2], &[4096]),
            Script::calm(),
            None,
            quick(Duration::from_secs(600)),
            Some(2),
        )
        .await
        .unwrap();
        assert!(
            results[0]
                .passes
                .iter()
                .all(|p| (p.requests, p.failures) == (2, 2))
        );
    }

    /// Catches the stopped-then-restored arm not stopping the cold store for the burst, not
    /// restoring it before the first poll (the poll would see nothing), or leaving it faulted:
    /// two passes refuse once each and restore twice each (after the burst, after the pass).
    #[tokio::test]
    async fn the_stopped_arm_refuses_for_the_burst_and_restores_before_the_poll() {
        let dir = temp("tb-c-stopped");
        let (instance, cold) = (FakeS3::start().await, FakeS3::start().await);
        let stores = [
            (Role::Instance, instance.endpoint()),
            (Role::Cold, cold.endpoint()),
        ];
        let ctx = testkit::ctx(dir.path(), &stores, Window::Busy);
        let switch = Arc::new(SwitchLog::default());
        let copying = copier(instance.clone(), &cold);
        let results = run_with(
            &ctx,
            &plan(&[1], &[4096]),
            Script::calm(),
            Some(switch.clone()),
            quick(Duration::from_secs(60)),
            Some(2),
        )
        .await
        .unwrap();
        copying.abort();
        let labels: Vec<&str> = results.iter().map(|r| r.arm.as_str()).collect();
        assert_eq!(labels, ["2x4kib-c1-healthy", "2x4kib-c1-stopped-restored"]);
        let modes = switch.modes();
        assert_eq!(modes.iter().filter(|m| **m == FaultMode::Refuse).count(), 2);
        assert_eq!(modes.iter().filter(|m| **m == FaultMode::Pass).count(), 4);
        assert_eq!(modes.last(), Some(&FaultMode::Pass));
        assert!(results[1].passes.iter().all(|p| p.failures == 0));
    }

    /// Catches a run that goes on without the instance or the cold role, and `run` doing
    /// anything about it.
    #[tokio::test]
    async fn both_roles_are_required() {
        let dir = temp("tb-c-roles");
        let fake = FakeS3::start().await;
        let only = |role| testkit::ctx(dir.path(), &[(role, fake.endpoint())], Window::Busy);
        for role in [Role::Instance, Role::Cold] {
            let err = run(&only(role), &plan(&[1], &[])).await.unwrap_err();
            assert!(matches!(err, BenchError::Config(_)), "{err}");
        }
    }

    /// Catches a request of the cell whose record cannot be written being taken for a good one
    /// (a put, a listing, a delete of either store): the log refuses from its n-th write, for
    /// every n, until a run is long enough not to meet it.
    #[tokio::test]
    async fn a_log_that_fails_at_any_request_stops_the_run() {
        let mut finished = false;
        for taken in 0..400 {
            let dir = temp("tb-c-log");
            let (instance, cold) = (FakeS3::start().await, FakeS3::start().await);
            let stores = [
                (Role::Instance, instance.endpoint()),
                (Role::Cold, cold.endpoint()),
            ];
            let healthy = testkit::ctx(dir.path(), &stores, Window::Busy);
            strays(&healthy, "1x4kib-c1-healthy", 2).await;
            let log = Arc::new(RequestLog::to_writer(testkit::FailAfter(taken)));
            let ctx = testkit::ctx_with_log(dir.path(), &stores, Window::Busy, log);
            match run_with(
                &ctx,
                &plan(&[1], &[4096]),
                Script::calm(),
                None,
                quick(Duration::ZERO),
                Some(1),
            )
            .await
            {
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
