//! The arms of the two put cells, [`crate::cells::cold_put`] and [`crate::cells::a1`]: they
//! measure the same thing, put-to-ack, on different paths.
//!
//! An arm is one path (a store and a way of putting), one object size, one pace (closed loop at
//! some requests in flight, or open loop at a fixed rate) and one mode of the cold store (healthy,
//! or a fault the proxy adds). Each pass puts `requests` objects of the same seeded bytes under
//! `<prefix>/<arm>/p<pass>/<index>`, then, with the proxy back in pass mode, deletes them and
//! aborts the uploads they left. Latency is put-to-ack: from the first request of the put until
//! the last one is answered (the sum of the requests' times, for a multipart put).
//!
//! The counts and sizes are the plan's; an operator narrows them with `--object-bytes` and
//! `--in-flight`, leaves out the open-loop arms with `--open-loop off`, and sets the count of a
//! multipart path's closed-loop arms with `--multipart-objects`. The ledger refuses
//! what does not fit the caps, and is asked about the arms built, so a plan without the open-loop
//! arms costs without them.

use std::sync::Arc;

use crate::cells::content;
use crate::cells::engine::{Arm, Cap, Env, FaultSwitch, Pace, Step, hook, slug, work};
use crate::cells::modes::{Mode, put_modes, width_ok};
use crate::cells::stats;
use crate::cells::{CellId, CellPlan};
use crate::error::BenchError;
use crate::proxy::FaultMode;
use crate::result::Op;
use crate::role::Role;
use crate::s3::BenchS3;

use crate::cells::modes::{KIB, MIB};

/// Sizes of the plan when `--object-bytes` names none.
pub const DEFAULT_SIZES: [u64; 6] = [4 * KIB, 19 * KIB, 64 * KIB, MIB, 16 * MIB, 128 * MIB];

/// One way of putting an object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PutPath {
    /// The name in the arm label.
    pub name: &'static str,
    /// The store the requests go to.
    pub role: Role,
    /// Whether the object goes as a multipart upload (to a tierstore front, the path that does
    /// not touch the hot tier) or as one PUT.
    pub multipart: bool,
    /// Whether the object ends up on the cold store, for the ledger.
    pub cold: bool,
}

/// `4kib`, `1mib`, `100b`: a size as an arm label has it.
#[must_use]
pub fn size_label(bytes: u64) -> String {
    if bytes.is_multiple_of(MIB) {
        format!("{}mib", bytes / MIB)
    } else if bytes.is_multiple_of(KIB) {
        format!("{}kib", bytes / KIB)
    } else {
        format!("{bytes}b")
    }
}

/// Requests per pass at `size` bytes with `in_flight` workers: 1,000 up to 1 MiB, 100 at
/// 16 MiB (64 at 16 in flight or more), 12 above (16 at 16 in flight or more).
#[must_use]
pub fn requests_for(size: u64, in_flight: u32) -> u64 {
    let (narrow, wide) = if size <= MIB {
        (1000, 1000)
    } else if size <= 16 * MIB {
        (100, 64)
    } else {
        (12, 16)
    };
    if in_flight >= 16 { wide } else { narrow }
}

/// The note an arm with `requests` objects per pass carries when no p99 may be claimed from it:
/// below [`stats::P99_MIN_N`] a p99 is the slowest request or close to it. `None` from that many
/// requests on.
#[must_use]
pub fn p99_note(requests: u64) -> Option<String> {
    let least = u64::try_from(stats::P99_MIN_N).unwrap_or(u64::MAX);
    (requests < least).then(|| {
        format!("p99 not reported: {requests} objects per pass, fewer than the 1,000 a p99 needs")
    })
}

/// The open-loop arms: `(size, requests per second, seconds)`.
const OPEN_LOOP: [(u64, u32, u64); 2] = [(19 * KIB, 20, 600), (MIB, 10, 300)];

/// What one arm needs to put.
#[derive(Clone)]
struct Putter {
    s3: Arc<BenchS3>,
    path: PutPath,
    size: u64,
    body: bytes::Bytes,
    prefix: String,
}

impl Putter {
    fn key(&self, pass: u32, index: u64) -> String {
        format!("{}/p{pass}/{index}", self.prefix)
    }

    async fn put(&self, pass: u32, index: u64) -> Result<Vec<Step>, BenchError> {
        let key = self.key(pass, index);
        let outcome = if self.path.multipart {
            self.s3.put_multipart(&key, self.body.clone()).await?
        } else {
            self.s3.put(&key, self.body.clone()).await?
        };
        let written = if self.path.cold { self.size } else { 0 };
        Ok(vec![
            Step::of(self.path.role, Op::Put, self.size, &outcome).writing(written),
        ])
    }

    /// Deletes what pass `pass` of `requests` puts may have written, and the uploads it left.
    async fn clean(&self, pass: u32, requests: u64) -> Result<Vec<String>, BenchError> {
        let mut deleted = 0u64;
        for index in 0..requests {
            let outcome = self.s3.delete(&self.key(pass, index)).await?;
            deleted += u64::from(outcome.is_ok());
        }
        let aborted = self
            .s3
            .abort_uploads(&format!("{}/p{pass}/", self.prefix))
            .await?;
        Ok(vec![format!(
            "cleanup: {deleted} of {requests} deletes answered, {aborted} unfinished uploads aborted"
        )])
    }
}

/// The notes every arm of a put cell carries, for the path and the mode.
fn notes(cell: CellId, path: &PutPath, mode: &Mode, size: u64, requests: u64) -> Vec<String> {
    let how = match (path.multipart, path.role) {
        (true, Role::Instance) => {
            "a multipart upload through the instance: its parts go straight to the cold tier and do not touch the hot tier. The instance has no cold-only mode at the pinned tag, so it runs with a hot tier of its own on the same host (two-tier mode); this arm measures the cold path of it"
        }
        (true, _) => {
            "a multipart upload straight to the store, the floor for the instance's multipart path"
        }
        (false, Role::Instance) => {
            "a plain put through the instance, acknowledged from its hot tier; the copy to the cold tier follows and its bytes are counted against the cold ledger"
        }
        (false, _) => "a single PUT straight to the store, the floor",
    };
    let fault = mode.fault.map_or_else(
        || "none: the cold store as it is".to_owned(),
        |f| format!("{f:?} through the fault proxy (the arm of mode proxy-pass is its control)"),
    );
    let mut notes = vec![
        format!("{}: put-to-ack by {}, {how}", cell.as_str(), path.name),
        format!("test instance and test buckets; fault: {fault}"),
        format!(
            "every object of the arm has the same {} seeded bytes; keys are unique per pass",
            size_label(size)
        ),
    ];
    notes.extend(p99_note(requests));
    notes
}

/// The arms of a put cell: every path the run configures, at every size, in flight and mode.
///
/// # Errors
/// [`BenchError`] if no path of `paths` has its role configured, or a client cannot be made.
pub fn arms(
    env: &Env,
    cell: CellId,
    paths: &[PutPath],
    plan: &CellPlan,
    faults: Option<Arc<dyn FaultSwitch>>,
    limit: Option<u64>,
) -> Result<Vec<Arm>, BenchError> {
    let sizes: Vec<u64> = if plan.object_bytes.is_empty() {
        DEFAULT_SIZES.to_vec()
    } else {
        plan.object_bytes.clone()
    };
    let smallest = sizes.iter().copied().min().unwrap_or(KIB);
    let mut out = Vec::new();
    let mut any = false;
    for path in paths
        .iter()
        .filter(|p| env.config.stores.contains_key(&p.role))
    {
        any = true;
        let cap = if path.role == Role::Hot {
            Cap::Hot
        } else {
            Cap::Cold
        };
        let s3 = env.s3(path.role, cap)?;
        for mode in put_modes(faults.is_some()) {
            for &size in &sizes {
                if !mode.wants(size, smallest) {
                    continue;
                }
                let body = content::body(env.config.seed ^ size, size);
                let flights: Vec<u32> = mode
                    .in_flight
                    .map_or_else(|| plan.in_flight.clone(), |n| vec![n]);
                for &in_flight in flights.iter().filter(|&&n| width_ok(n, size)) {
                    // `--multipart-objects` replaces the plan's count of a multipart path; the
                    // fault modes' own counts are small already and stay.
                    let planned = |size, in_flight| {
                        plan.multipart_objects
                            .filter(|_| path.multipart)
                            .unwrap_or_else(|| requests_for(size, in_flight))
                    };
                    let requests = mode
                        .requests(size, in_flight, planned)
                        .min(limit.unwrap_or(u64::MAX));
                    let label = slug(&format!(
                        "{}-{}-c{in_flight}-{}",
                        path.name,
                        size_label(size),
                        mode.name
                    ));
                    let putter = Putter {
                        s3: s3.clone(),
                        path: *path,
                        size,
                        body: body.clone(),
                        prefix: format!("{}/{}", env.prefix(), label),
                    };
                    out.push(arm(
                        label,
                        Pace::Closed,
                        in_flight,
                        requests,
                        notes(cell, path, &mode, size, requests),
                        putter,
                        mode.fault.zip(faults.clone()),
                    ));
                }
                let open = OPEN_LOOP.iter().filter(|(s, ..)| *s == size);
                // `&`, not `&&`: both are plain flags, and a short circuit would be a branch.
                let wanted = plan.open_loop & mode.fault.is_none();
                for &(_, per_sec, secs) in open.filter(|_| wanted) {
                    let requests = (u64::from(per_sec) * secs).min(limit.unwrap_or(u64::MAX));
                    let label = slug(&format!(
                        "{}-{}-open{per_sec}-{}",
                        path.name,
                        size_label(size),
                        mode.name
                    ));
                    let putter = Putter {
                        s3: s3.clone(),
                        path: *path,
                        size,
                        body: body.clone(),
                        prefix: format!("{}/{}", env.prefix(), label),
                    };
                    let mut notes = notes(cell, path, &mode, size, requests);
                    notes.push(format!(
                        "open loop: {per_sec} puts per second for {secs} s; a put that goes out late has its delay in its latency"
                    ));
                    out.push(arm(
                        label,
                        Pace::Open { per_sec },
                        0,
                        requests,
                        notes,
                        putter,
                        None,
                    ));
                }
            }
        }
    }
    if any {
        Ok(out)
    } else {
        Err(BenchError::Setup(format!(
            "{}: none of the roles {} is configured",
            cell.as_str(),
            paths
                .iter()
                .map(|p| p.role.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )))
    }
}

fn arm(
    label: String,
    pace: Pace,
    in_flight: u32,
    requests: u64,
    notes: Vec<String>,
    putter: Putter,
    fault: Option<(FaultMode, Arc<dyn FaultSwitch>)>,
) -> Arm {
    let cold_bytes_per_pass = if putter.path.cold {
        requests * putter.size
    } else {
        0
    };
    let sender = putter.clone();
    let before = fault.clone().map(|(mode, switch)| {
        hook(move |_| std::future::ready(switch.set_mode(mode).map(|()| vec![])))
    });
    let faulted = fault
        .as_ref()
        .is_some_and(|(mode, _)| *mode != FaultMode::Pass);
    let switch = fault.map(|(_, switch)| switch);
    let proxy = switch.clone();
    Arm {
        label,
        pace,
        in_flight,
        requests,
        cold_bytes_per_pass,
        cold_bytes_once: 0,
        work: work(move |unit| {
            let sender = sender.clone();
            async move { sender.put(unit.pass, unit.index).await }
        }),
        before,
        after: Some(hook(move |pass| {
            let putter = putter.clone();
            let switch = switch.clone();
            async move {
                if let Some(switch) = &switch {
                    switch.set_mode(FaultMode::Pass)?;
                }
                putter.clean(pass, requests).await
            }
        })),
        faulted,
        notes,
        proxy,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cells::cold_put::PATHS;
    use crate::cells::testkit::{self, Script};
    use crate::result::Window;

    fn env(stores: &[(Role, &str)]) -> (kbf_testkit::dir::TestDir, Env) {
        let dir = kbf_testkit::dir::TestDir::under(std::env::temp_dir(), "tb-putcell");
        let ctx = testkit::ctx(dir.path(), stores, Window::Busy);
        let env = Env::new(&ctx, Script::calm()).unwrap();
        (dir, env)
    }

    fn plan(in_flight: &[u32], sizes: &[u64]) -> CellPlan {
        CellPlan {
            passes: 2,
            in_flight: in_flight.to_vec(),
            object_bytes: sizes.to_vec(),
            open_loop: true,
            multipart_objects: None,
        }
    }

    fn with_multipart_objects(mut plan: CellPlan, n: u64) -> CellPlan {
        plan.multipart_objects = Some(n);
        plan
    }

    fn summary(arms: &[Arm]) -> Vec<(String, u64, u32)> {
        let mut rows: Vec<_> = arms
            .iter()
            .map(|a| (a.label.clone(), a.requests, a.in_flight))
            .collect();
        rows.sort();
        rows
    }

    /// Catches a size label that is not one a key can carry, or that two sizes share.
    #[test]
    fn a_size_label_is_its_unit_or_its_bytes() {
        assert_eq!(size_label(4096), "4kib");
        assert_eq!(size_label(19 * 1024), "19kib");
        assert_eq!(size_label(MIB), "1mib");
        assert_eq!(size_label(128 * MIB), "128mib");
        assert_eq!(size_label(100), "100b");
    }

    /// Catches a count of the plan that drifted: 1,000 puts up to 1 MiB at any width, 100 and
    /// 12 narrow, 64 and 16 at 16 in flight or more.
    #[test]
    fn the_request_counts_are_the_plans() {
        for in_flight in [1, 16, 64] {
            assert_eq!(requests_for(4096, in_flight), 1000);
            assert_eq!(requests_for(MIB, in_flight), 1000);
        }
        assert_eq!(requests_for(16 * MIB, 1), 100);
        assert_eq!(requests_for(16 * MIB, 16), 64);
        assert_eq!(requests_for(128 * MIB, 1), 12);
        assert_eq!(requests_for(128 * MIB, 64), 16);
    }

    /// Catches arms that are missing, doubled or mislabelled: every path of a configured role at
    /// every size and width, the open-loop arms for 19 KiB and 1 MiB only, 64 in flight only up
    /// to 1 MiB, and the cold bytes of a pass that the ledger will be asked for.
    #[test]
    fn the_healthy_arms_are_the_paths_sizes_and_widths_asked_for() {
        let (_dir, env) = env(&[(Role::Cold, "http://127.0.0.1:9")]);
        let sizes = [4096, 19 * 1024, 16 * MIB];
        let arms = arms(
            &env,
            CellId::ColdPut,
            &PATHS,
            &plan(&[1, 64], &sizes),
            None,
            None,
        )
        .unwrap();
        let rows = summary(&arms);
        let labels: Vec<&str> = rows.iter().map(|r| r.0.as_str()).collect();
        assert_eq!(
            labels,
            [
                "direct-16mib-c1-healthy",
                "direct-19kib-c1-healthy",
                "direct-19kib-c64-healthy",
                "direct-19kib-open20-healthy",
                "direct-4kib-c1-healthy",
                "direct-4kib-c64-healthy",
                "direct-mp-16mib-c1-healthy",
                "direct-mp-19kib-c1-healthy",
                "direct-mp-19kib-c64-healthy",
                "direct-mp-19kib-open20-healthy",
                "direct-mp-4kib-c1-healthy",
                "direct-mp-4kib-c64-healthy",
            ]
        );
        assert_eq!(rows[0], ("direct-16mib-c1-healthy".to_owned(), 100, 1));
        assert_eq!(
            rows[3],
            ("direct-19kib-open20-healthy".to_owned(), 12_000, 0)
        );
        let open = arms.iter().find(|a| a.label.contains("open")).unwrap();
        assert_eq!(open.pace, Pace::Open { per_sec: 20 });
        assert_eq!(open.cold_bytes_per_pass, 12_000 * 19 * 1024);
        assert!(
            open.notes
                .iter()
                .any(|n| n.contains("open loop: 20 puts per second for 600 s"))
        );
        assert!(arms.iter().all(|a| a.before.is_none()));
        assert!(arms.iter().all(|a| a.after.is_some()));
    }

    /// Catches a 1 MiB open loop at the wrong rate, and a limit that does not cap the count of a
    /// pass.
    #[test]
    fn the_one_mib_open_loop_is_ten_a_second_for_five_minutes_and_a_limit_caps_a_pass() {
        let (_dir, env) = env(&[(Role::Instance, "http://127.0.0.1:9")]);
        let arms = arms(
            &env,
            CellId::ColdPut,
            &PATHS,
            &plan(&[1], &[MIB]),
            None,
            None,
        )
        .unwrap();
        let open = arms.iter().find(|a| a.label.contains("open")).unwrap();
        assert_eq!(
            (open.label.as_str(), open.requests),
            ("instance-1mib-open10-healthy", 3000)
        );
        assert!(open.notes[0].contains("through the instance"));
        assert!(open.notes[0].contains("two-tier"));
        let capped = self::arms(
            &env,
            CellId::ColdPut,
            &PATHS,
            &plan(&[1], &[MIB]),
            None,
            Some(5),
        )
        .unwrap();
        assert!(capped.iter().all(|a| a.requests == 5));
    }

    /// Catches the multipart count leaking into a single-PUT arm, not reaching a multipart one
    /// (at any width, size or role), reaching the open-loop arms (their count is a rate times a
    /// time), or the ledger being asked for the plan's bytes and not the arm's own: with 7
    /// objects, `direct` keeps 1,000 and 100, both multipart paths have 7, and each arm's cold
    /// bytes are its own count times its size.
    #[test]
    fn the_multipart_count_is_the_multipart_arms_own_and_the_ledger_follows_it() {
        let (_dir, env) = env(&[
            (Role::Cold, "http://127.0.0.1:9"),
            (Role::Instance, "http://127.0.0.1:9"),
        ]);
        let plan = with_multipart_objects(plan(&[1, 16], &[19 * 1024, 16 * MIB]), 7);
        let arms = arms(&env, CellId::ColdPut, &PATHS, &plan, None, None).unwrap();
        let closed: Vec<(&str, u64, u64)> = arms
            .iter()
            .filter(|a| !matches!(a.pace, Pace::Open { .. }))
            .map(|a| (a.label.as_str(), a.requests, a.cold_bytes_per_pass))
            .collect();
        let size = |label: &str| {
            if label.contains("19kib") {
                19 * 1024
            } else {
                16 * MIB
            }
        };
        for (label, requests, cold_bytes) in &closed {
            let want = if label.starts_with("direct-") && !label.starts_with("direct-mp") {
                requests_for(size(label), if label.contains("c16") { 16 } else { 1 })
            } else {
                7
            };
            assert_eq!(*requests, want, "{label}");
            assert_eq!(*cold_bytes, want * size(label), "{label}");
        }
        assert_eq!(closed.len(), 12);
        let open: Vec<(&str, u64)> = arms
            .iter()
            .filter(|a| matches!(a.pace, Pace::Open { .. }))
            .map(|a| (a.label.as_str(), a.requests))
            .collect();
        assert_eq!(
            open,
            [
                ("direct-19kib-open20-healthy", 12_000),
                ("direct-mp-19kib-open20-healthy", 12_000),
                ("instance-19kib-open20-healthy", 12_000),
            ]
        );
    }

    /// Catches the flag raising an arm: it is a cap, so with 200 a 4 KiB multipart arm (plan
    /// 1,000) is cut to 200, and a 16 MiB one (plan 100 narrow, 64 wide) keeps its smaller plan.
    #[test]
    fn the_multipart_count_caps_an_arm_and_never_raises_it() {
        let (_dir, env) = env(&[(Role::Cold, "http://127.0.0.1:9")]);
        let plan = with_multipart_objects(plan(&[1, 16], &[4096, 16 * MIB]), 200);
        let arms = arms(&env, CellId::ColdPut, &PATHS, &plan, None, None).unwrap();
        let mut counts: Vec<(&str, u64)> = arms
            .iter()
            .filter(|a| a.label.starts_with("direct-mp") & !a.label.contains("open"))
            .map(|a| (a.label.as_str(), a.requests))
            .collect();
        counts.sort_unstable();
        assert_eq!(
            counts,
            [
                ("direct-mp-16mib-c1-healthy", 100),
                ("direct-mp-16mib-c16-healthy", 64),
                ("direct-mp-4kib-c1-healthy", 200),
                ("direct-mp-4kib-c16-healthy", 200),
            ]
        );
    }

    /// Catches a p99 note that is missing below a thousand objects, or present from a thousand:
    /// the line is the one `P99_MIN_N` draws, and the note names the count.
    #[test]
    fn the_p99_note_is_there_below_a_thousand_objects_and_only_there() {
        assert_eq!(
            p99_note(999).as_deref(),
            Some("p99 not reported: 999 objects per pass, fewer than the 1,000 a p99 needs")
        );
        assert_eq!(
            p99_note(12).as_deref(),
            Some("p99 not reported: 12 objects per pass, fewer than the 1,000 a p99 needs")
        );
        assert_eq!(p99_note(1000), None);
        assert_eq!(p99_note(12_000), None);
    }

    const GIB: u64 = 1 << 30;

    fn open(arms: &[Arm]) -> Vec<&str> {
        arms.iter()
            .filter(|a| matches!(a.pace, Pace::Open { .. }))
            .map(|a| a.label.as_str())
            .collect()
    }

    fn with_open_loop(mut plan: CellPlan, open_loop: bool) -> CellPlan {
        plan.open_loop = open_loop;
        plan
    }

    /// Catches an open-loop arm that cannot be left out, or a ledger that is asked about arms
    /// other than the ones built. Cold-put at 1 MiB, one in flight, six passes, the three paths:
    /// each path puts 1,000 objects of 1 MiB closed (1,048,576,000 bytes a pass) and, with the
    /// open loop, 3,000 more (3,145,728,000). On, the cell is 75,497,472,000 bytes against a cap
    /// of 20 GiB (21,474,836,480) and is refused; off, it is 18,874,368,000 and fits.
    #[test]
    fn cold_put_without_the_open_loop_has_none_and_a_one_mib_run_fits_the_cell_cap() {
        use crate::cells::engine::admission_cost;
        let (_dir, env) = env(&[
            (Role::Cold, "http://127.0.0.1:9"),
            (Role::Instance, "http://127.0.0.1:9"),
        ]);
        let build = |open_loop| {
            let plan = with_open_loop(plan(&[1], &[MIB]), open_loop);
            arms(&env, CellId::ColdPut, &PATHS, &plan, None, None).unwrap()
        };
        let (on, off) = (build(true), build(false));
        assert_eq!(
            open(&on),
            [
                "direct-1mib-open10-healthy",
                "direct-mp-1mib-open10-healthy",
                "instance-1mib-open10-healthy"
            ]
        );
        assert!(open(&off).is_empty(), "{:?}", open(&off));
        assert_eq!((on.len(), off.len()), (6, 3));
        let per_pass = |arms: &[Arm]| {
            arms.iter()
                .map(|a| a.cold_bytes_per_pass)
                .collect::<Vec<_>>()
        };
        assert_eq!(per_pass(&off), [1_048_576_000; 3]);
        assert_eq!(per_pass(&on).iter().sum::<u64>(), 12_582_912_000);
        assert_eq!(admission_cost(&on, 6), (3_145_728_000, 75_497_472_000));
        assert_eq!(admission_cost(&off, 6), (1_048_576_000, 18_874_368_000));
        assert!(admission_cost(&on, 6).1 > 20 * GIB);
        assert!(admission_cost(&off, 6).1 < 20 * GIB);
    }

    /// Catches the same for a1 (the hot baseline is not charged to the cold ledger, the instance
    /// is): at 1 MiB the instance's closed arm is 1,048,576,000 bytes a pass and its open arm
    /// 3,145,728,000, so six passes are 25,165,824,000 on (over the 20 GiB cell cap) and
    /// 6,291,456,000 off.
    #[test]
    fn a1_without_the_open_loop_has_none_and_its_cold_bytes_are_those_of_the_closed_arm() {
        use crate::cells::a1::PATHS as A1;
        use crate::cells::engine::admission_cost;
        let (_dir, env) = env(&[
            (Role::Hot, "http://127.0.0.1:9"),
            (Role::Instance, "http://127.0.0.1:9"),
        ]);
        let build = |open_loop| {
            let plan = with_open_loop(plan(&[1], &[MIB]), open_loop);
            arms(&env, CellId::A1, &A1, &plan, None, None).unwrap()
        };
        let (on, off) = (build(true), build(false));
        assert_eq!(
            open(&on),
            ["hot-1mib-open10-healthy", "instance-1mib-open10-healthy"]
        );
        assert!(open(&off).is_empty());
        assert_eq!(admission_cost(&on, 6), (3_145_728_000, 25_165_824_000));
        assert_eq!(admission_cost(&off, 6), (1_048_576_000, 6_291_456_000));
        assert!(admission_cost(&on, 6).1 > 20 * GIB);
        assert!(admission_cost(&off, 6).1 < 20 * GIB);
    }

    /// Catches an off switch that takes away only the 1 MiB arm, or that also takes away a
    /// closed-loop arm: at 19 KiB and 1 MiB, off leaves exactly the arms on has less its open ones.
    #[test]
    fn off_removes_every_open_arm_at_every_size_and_nothing_else() {
        let (_dir, env) = env(&[(Role::Cold, "http://127.0.0.1:9")]);
        let sizes = [19 * 1024, MIB];
        let build = |open_loop| {
            let plan = with_open_loop(plan(&[1], &sizes), open_loop);
            arms(&env, CellId::ColdPut, &PATHS[..1], &plan, None, None).unwrap()
        };
        let (on, off) = (build(true), build(false));
        assert_eq!(
            open(&on),
            ["direct-19kib-open20-healthy", "direct-1mib-open10-healthy"]
        );
        let labels = |arms: &[Arm]| arms.iter().map(|a| a.label.clone()).collect::<Vec<_>>();
        let closed: Vec<String> = labels(&on)
            .into_iter()
            .filter(|l| !l.contains("-open"))
            .collect();
        assert_eq!(labels(&off), closed);
        assert!(off.iter().all(|a| a.pace == Pace::Closed));
    }

    /// Catches a run configured with none of the roles of the cell going on to send nothing.
    #[test]
    fn a_run_with_none_of_the_roles_is_refused() {
        let (_dir, env) = env(&[(Role::Hot, "http://127.0.0.1:9")]);
        let err = arms(&env, CellId::ColdPut, &PATHS, &plan(&[1], &[]), None, None)
            .err()
            .unwrap();
        assert!(
            err.to_string()
                .contains("cold-put: none of the roles cold, cold, instance"),
            "{err}"
        );
    }

    /// Catches fault arms that are not the plan's: each mode at the sizes it is run at, the
    /// counts and widths of the slow and the stopped, the control at every size, and the notes
    /// that name the fault.
    #[test]
    fn the_fault_arms_are_the_plans() {
        let (_dir, env) = env(&[(Role::Cold, "http://127.0.0.1:9")]);
        let switch: Arc<dyn FaultSwitch> = Arc::new(testkit::SwitchLog::default());
        let sizes = [4096, MIB, 16 * MIB];
        let arms = arms(
            &env,
            CellId::ColdPut,
            &PATHS[..1],
            &plan(&[1], &sizes),
            Some(switch),
            None,
        )
        .unwrap();
        let rows = summary(&arms);
        let want = |label: &str, requests, in_flight| (label.to_owned(), requests, in_flight);
        assert_eq!(
            rows,
            [
                want("direct-16mib-c1-proxy-pass", 100, 1),
                want("direct-16mib-c1-latency-500ms", 20, 1),
                want("direct-16mib-c1-latency-50ms", 20, 1),
                want("direct-16mib-c1-bandwidth-10mib-s", 10, 1),
                want("direct-1mib-c1-proxy-pass", 1000, 1),
                want("direct-1mib-c1-latency-500ms", 100, 1),
                want("direct-1mib-c1-latency-50ms", 100, 1),
                want("direct-1mib-c1-bandwidth-10mib-s", 50, 1),
                want("direct-4kib-c6-blackhole", 30, 6),
                want("direct-4kib-c6-refused", 30, 6),
                want("direct-4kib-c1-proxy-pass", 1000, 1),
                want("direct-4kib-c1-latency-500ms", 200, 1),
                want("direct-4kib-c1-latency-50ms", 200, 1),
            ]
            .iter()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
        );
        assert!(arms.iter().all(|a| a.before.is_some()));
        assert!(arms.iter().all(|a| a.after.is_some()));
        let slow = arms
            .iter()
            .find(|a| a.label.contains("latency-50ms"))
            .unwrap();
        assert!(
            slow.notes[1].contains("Latency { ms: 50 }"),
            "{:?}",
            slow.notes
        );
    }
}
