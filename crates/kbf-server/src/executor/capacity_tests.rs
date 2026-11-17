use std::time::Duration;

use js_proto::v1::{Host, HostMark, HostState, Shape};
use kbf_job_client::fake::{Call, Fault};
use tokio::time::Instant;
use tonic::Code;

use super::{MAX_SLOTS, SIZE_EVERY, Sizing, fleet_cores, per_worker};
use crate::executor::test_support::{Harness, Log, machine};

/// A live, unmarked Linux host of `cores` cores on `x86_64`.
fn host(cores: u64) -> Host {
    Host {
        id: format!("h{cores}"),
        state: HostState::Live.into(),
        shape: Some(Shape {
            cpu_millis: cores * 1_000 + 999,
            ..Shape::default()
        }),
        capabilities: [
            ("os".to_owned(), "linux".to_owned()),
            ("arch".to_owned(), "x86_64".to_owned()),
        ]
        .into(),
        ..Host::default()
    }
}

/// Catches a host counted that takes no ordinary job of the executor, or one left out that
/// does: only the whole cores of live hosts with no mark, no cordon and no taint, of `linux`
/// and the executor's architecture (or naming neither), count.
#[test]
fn only_the_hosts_that_take_the_executors_jobs_count_their_whole_cores() {
    let with = |change: fn(&mut Host)| {
        let mut host = host(1_000);
        change(&mut host);
        host
    };
    let counted = [
        host(16),
        with(|h| h.capabilities.clear()),
        // A host with no shape has no cores to count.
        with(|h| h.shape = None),
    ];
    assert_eq!(fleet_cores(&counted, "x86_64"), 16 + 1_000);
    let left_out = [
        with(|h| h.state = HostState::Lost.into()),
        with(|h| h.state = HostState::Awaiting.into()),
        with(|h| h.state = HostState::Unspecified.into()),
        with(|h| h.mark = HostMark::Draining.into()),
        with(|h| h.mark = HostMark::Reserved.into()),
        with(|h| h.cordon_expires_at_ms = Some(1)),
        with(|h| h.taint_reason = "dirty".to_owned()),
        with(|h| {
            h.capabilities.insert("os".to_owned(), "macos".to_owned());
        }),
        with(|h| {
            h.capabilities
                .insert("arch".to_owned(), "aarch64".to_owned());
        }),
    ];
    for host in &left_out {
        assert_eq!(
            fleet_cores(std::slice::from_ref(host), "x86_64"),
            0,
            "{host:?}"
        );
    }
    assert_eq!(fleet_cores(&[], "x86_64"), 0);
}

/// Catches a share that cuts a worker below the declared slots, above the most a worker
/// takes, or rounds a remainder away: the cores are spread over the workers rounded up,
/// clamped to the declared slots and to 256.
#[test]
fn a_workers_share_is_rounded_up_and_kept_between_the_declared_slots_and_the_most() {
    assert_eq!(per_worker(33, 2, 8), 17);
    assert_eq!(per_worker(32, 2, 8), 16);
    assert_eq!(per_worker(10, 2, 8), 8);
    assert_eq!(per_worker(0, 4, 8), 8);
    assert_eq!(per_worker(100_000, 1, 8), MAX_SLOTS);
    assert_eq!(per_worker(40, 0, 2), 40);
}

/// The calls the job service took to list its hosts.
fn listings(harness: &Harness) -> usize {
    let calls = harness.fake.calls();
    calls
        .iter()
        .filter(|call| matches!(call, Call::ListHosts(_)))
        .count()
}

/// Catches a size not taken from the hosts, or hosts read on every pass: the first check reads
/// the fake's one 16-core host (16 slots for the one worker), a check before [`SIZE_EVERY`] has
/// passed reads nothing, and the next one reads again, finding a second host.
#[tokio::test(start_paused = true)]
async fn the_hosts_are_read_once_each_period_and_size_the_workers() {
    let harness = Harness::new().await;
    let ctx = harness.ctx();
    let mut sizing = Sizing::new(harness.cfg.slots);
    let now = Instant::now();
    sizing.check(&ctx, now).await;
    assert_eq!((sizing.slots, listings(&harness)), (16, 1));
    harness.fake.add_host("host-2", machine());
    sizing
        .check(&ctx, now + SIZE_EVERY - Duration::from_millis(1))
        .await;
    assert_eq!((sizing.slots, listings(&harness)), (16, 1));
    sizing.check(&ctx, now + SIZE_EVERY).await;
    assert_eq!((sizing.slots, listings(&harness)), (32, 2));
}

/// Catches a size lost, or a warning repeated on every period, when the hosts cannot be read:
/// a listing the service refuses keeps the last size and is logged once, a second refusal in a
/// row is not logged, and after a listing that succeeds the next refusal is logged again.
#[tokio::test(start_paused = true)]
async fn a_listing_that_fails_keeps_the_size_and_is_logged_once_a_run_of_failures() {
    let harness = Harness::new().await;
    let ctx = harness.ctx();
    let mut sizing = Sizing::new(harness.cfg.slots);
    let refuse = || {
        harness.fake.inject_on(
            "ListHosts",
            Fault::Status {
                code: Code::PermissionDenied,
                message: "not for this client".to_owned(),
            },
        );
    };
    let start = Instant::now();
    let mut warnings = Vec::new();
    for (n, refused) in [false, true, true, false, true].into_iter().enumerate() {
        if refused {
            refuse();
        }
        let log = Log::default();
        let guard = log.install();
        let at = start + SIZE_EVERY * u32::try_from(n).expect("a few");
        sizing.check(&ctx, at).await;
        drop(guard);
        assert_eq!(sizing.slots, 16, "check {n}");
        warnings.push(log.text().contains("could not be read"));
    }
    assert_eq!(warnings, [false, true, false, false, true]);
}
