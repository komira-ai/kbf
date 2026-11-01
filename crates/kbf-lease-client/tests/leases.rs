//! The lease routes through the client, against the fake's admission rules.

mod support;

use std::time::Duration;

use kbf_lease_client::fake::FakeLeaseServer;
use kbf_lease_client::{Acquired, ErrorCode, HostLoad, LeaseState, ReleaseState, Resources};
use support::{fake, lease, pool, refused, run_of, whole_host};

/// Asserts that a lease books a whole host, the next lease gets the next free host, and
/// that with every host booked the answer is `409 no-capacity` until a lease is released.
/// It catches a fake (or a client) that lets two leases share a host, and a client that turns
/// the 409 into a transport failure instead of the refusal a caller retries on.
#[tokio::test]
async fn each_lease_holds_a_whole_host_until_it_is_released() {
    let (_server, client) = fake().await;
    let Acquired::Granted(first) = client.acquire(&whole_host()).await.unwrap() else {
        panic!("a free host grants at once");
    };
    let Acquired::Granted(second) = client.acquire(&whole_host()).await.unwrap() else {
        panic!("a free host grants at once");
    };
    assert_eq!(
        (first.host.as_str(), second.host.as_str()),
        ("node1", "node2")
    );
    assert_ne!(first.id, second.id);
    let full = client.acquire(&whole_host()).await.unwrap_err();
    assert!(!full.may_have_applied());
    assert_eq!(refused(full), (409, ErrorCode::NoCapacity));

    assert_eq!(client.release(first.id).await.unwrap(), ReleaseState::Freed);
    let Acquired::Granted(third) = client.acquire(&whole_host()).await.unwrap() else {
        panic!("the freed host grants again");
    };
    assert_eq!(third.host, "node1");
}

/// Asserts the refusals of a request that breaks a rule, each with its status and code: the
/// wrong class, no wall time, an empty label key, a zero ttl (a 400 whose reason says where,
/// not what), more than a host holds, and a capability no host has. A client that handed back
/// a bare error here would leave the caller unable to tell a wrong request from a full farm.
#[tokio::test]
async fn a_request_that_breaks_a_rule_is_refused_with_its_code() {
    let (_server, client) = fake().await;
    let mut wrong_class = whole_host();
    wrong_class.class = "elsewhere".to_owned();
    let mut no_wall = whole_host();
    no_wall.max_wall_seconds = 0;
    let empty_label = whole_host().label("", "x");
    let mut too_big = whole_host();
    too_big.resources.memory_bytes += 1;
    let mut too_much_cpu = whole_host();
    too_much_cpu.resources.cpu_millis += 1;
    let mut too_many_gpus = whole_host();
    too_many_gpus.resources.gpus = 1;
    too_many_gpus.resources.gpu_class = Some("a100".to_owned());
    let mut no_such = whole_host();
    no_such
        .capabilities
        .insert("os".to_owned(), "macos".to_owned());
    for (request, status, code) in [
        (wrong_class, 422, ErrorCode::UnknownClass),
        (no_wall, 422, ErrorCode::WallTimeNotPositive),
        (empty_label, 422, ErrorCode::InvalidLabel),
        (too_big, 409, ErrorCode::NoFit),
        (too_much_cpu, 409, ErrorCode::NoFit),
        (too_many_gpus, 409, ErrorCode::NoFit),
        (no_such, 409, ErrorCode::NoCapableHost),
    ] {
        let error = client.acquire(&request).await.unwrap_err();
        assert_eq!(refused(error), (status, code));
    }
    let zero_ttl = whole_host().ttl(Duration::ZERO);
    let error = client.acquire(&zero_ttl).await.unwrap_err();
    let api = error.api().unwrap();
    assert_eq!((api.status, &api.code), (400, &ErrorCode::BadRequest));
    assert_eq!(api.reason.as_deref(), Some("ttl_seconds must be positive"));
    assert!(error.to_string().contains("ttl_seconds must be positive"));
}

/// Asserts the 202 path: a lease on a host that must be prepared is booked and listed
/// `pending`, takes no run (`409 not-held`) and cannot be renewed (`409 not-held`) until the
/// host is ready, and is `held` after. It catches a client that reads a 202 as a grant, and a
/// caller that would queue on a pending lease and be told so only by a hang.
#[tokio::test]
async fn a_pending_lease_takes_nothing_until_it_is_held() {
    let mut pool = pool();
    pool.prepare = true;
    let server = FakeLeaseServer::start(pool).await;
    let client = server.client().unwrap();
    let Acquired::Pending(pending) = client.acquire(&whole_host()).await.unwrap() else {
        panic!("a lease that must be prepared is pending");
    };
    assert_eq!(pending.state, LeaseState::Pending);
    let listed = client.leases().await.unwrap();
    assert_eq!(
        (listed[0].id, listed[0].state),
        (pending.id, LeaseState::Pending)
    );
    let queued = client
        .queue_run(pending.id, &run_of(1000, 1))
        .await
        .unwrap_err();
    assert_eq!(refused(queued), (409, ErrorCode::NotHeld));
    let renewed = client.renew(pending.id, None).await.unwrap_err();
    assert_eq!(refused(renewed), (409, ErrorCode::NotHeld));

    assert!(server.ready(pending.id));
    assert!(
        !server.ready(pending.id),
        "ready is for a pending lease only"
    );
    assert_eq!(client.leases().await.unwrap()[0].state, LeaseState::Held);
    client
        .queue_run(pending.id, &run_of(1000, 1))
        .await
        .unwrap();
}

/// Asserts how renew moves the expiry: by the ttl asked (the server's default when none),
/// never past the server's one-hour maximum ttl and never past the lease's own end. It
/// catches a renew that extends a lease beyond its maximum wall time, which would keep a
/// host booked after the caller's budget for it.
#[tokio::test]
async fn renew_moves_the_expiry_within_the_maximum_ttl_and_the_lease_end() {
    let (server, client) = fake().await;
    let mut short = whole_host();
    short.max_wall_seconds = 5400;
    let Acquired::Granted(granted) = client.acquire(&short).await.unwrap() else {
        panic!("granted");
    };
    let start = server.now_ms();
    assert_eq!(granted.expires_at_ms, start + 600_000);

    server.advance(Duration::from_secs(300));
    let renewed = client.renew(granted.id, None).await.unwrap();
    assert_eq!(renewed.expires_at_ms, start + 300_000 + 600_000);
    assert!(!renewed.host_reserved);
    let renewed = client
        .renew(granted.id, Some(Duration::from_secs(7200)))
        .await
        .unwrap();
    assert_eq!(
        renewed.expires_at_ms,
        start + 300_000 + 3_600_000,
        "the maximum ttl"
    );
    server.advance(Duration::from_secs(3000));
    let renewed = client
        .renew(granted.id, Some(Duration::from_secs(3600)))
        .await
        .unwrap();
    assert_eq!(
        renewed.expires_at_ms,
        start + 5_400_000,
        "the end of the lease"
    );
    let zero = client
        .renew(granted.id, Some(Duration::ZERO))
        .await
        .unwrap_err();
    assert_eq!(refused(zero), (400, ErrorCode::BadRequest));
}

/// Asserts that a lease nobody renews is dropped by the server's clock: it leaves the list,
/// its host is free again, a renew says `404 unknown-lease` and its runs are gone with it. It
/// catches a caller that keeps working on a lease the farm took back: the 404 is the signal.
#[tokio::test]
async fn a_lease_that_is_not_renewed_expires_and_frees_its_host() {
    let (server, client) = fake().await;
    let id = lease(&client).await;
    let run = client.queue_run(id, &run_of(1000, 1)).await.unwrap().run_id;
    server.advance(Duration::from_secs(599));
    client.renew(id, None).await.unwrap();
    server.advance(Duration::from_secs(600));
    assert!(client.leases().await.unwrap().is_empty());
    let renew = client.renew(id, None).await.unwrap_err();
    assert_eq!(refused(renew), (404, ErrorCode::UnknownLease));
    let gone = client.run(id, &run).await.unwrap_err();
    assert_eq!(refused(gone), (404, ErrorCode::UnknownLease));
    assert_eq!(lease(&client).await.to_string(), "4-2");
}

/// Asserts that a restarted caller finds its own leases by label and no one else's. It
/// catches a filter on the key alone, and one that returns every lease.
#[tokio::test]
async fn leases_are_found_by_label() {
    let (_server, client) = fake().await;
    client
        .acquire(&whole_host().label("kbf-frontend", "fe-1"))
        .await
        .unwrap();
    client
        .acquire(&whole_host().label("kbf-frontend", "fe-2"))
        .await
        .unwrap();
    assert_eq!(client.leases().await.unwrap().len(), 2);
    let mine = client
        .leases_labelled("kbf-frontend", "fe-1")
        .await
        .unwrap();
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0].labels["kbf-frontend"], "fe-1");
    assert_eq!(mine[0].state, LeaseState::Held);
    assert!(mine[0].ends_at_ms > mine[0].expires_at_ms);
    assert!(
        client
            .leases_labelled("kbf-frontend", "fe-3")
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        client
            .leases_labelled("other", "fe-1")
            .await
            .unwrap()
            .is_empty()
    );
}

/// Asserts that releasing a lease with live runs is `releasing` (202), asks its runs to stop,
/// keeps the host booked until they have ended, and then frees it; and that releasing a lease
/// the server does not have is `404 unknown-lease`. It catches a release that frees the host
/// while a run still uses it (the next lease would start on a busy host).
#[tokio::test]
async fn releasing_a_lease_with_live_runs_waits_for_them() {
    let (server, client) = fake().await;
    let id = lease(&client).await;
    let running = client.queue_run(id, &run_of(1000, 1)).await.unwrap().run_id;
    let also_running = client.queue_run(id, &run_of(1000, 1)).await.unwrap().run_id;
    let queued = client.queue_run(id, &run_of(1000, 1)).await.unwrap().run_id;
    assert!(server.run_started(&running));
    assert!(server.run_started(&also_running));

    assert_eq!(client.release(id).await.unwrap(), ReleaseState::Releasing);
    assert_eq!(
        client.release(id).await.unwrap(),
        ReleaseState::Releasing,
        "idempotent"
    );
    assert_eq!(
        client.leases().await.unwrap()[0].state,
        LeaseState::Releasing
    );
    let states =
        |runs: Vec<kbf_lease_client::Run>| runs.iter().map(|r| r.state).collect::<Vec<_>>();
    use kbf_lease_client::RunState::{NeverStarted, Stopping};
    assert_eq!(
        states(client.runs(id).await.unwrap()),
        [Stopping, Stopping, NeverStarted]
    );
    let renew = client.renew(id, None).await.unwrap_err();
    assert_eq!(refused(renew), (409, ErrorCode::NotHeld));
    let full = (
        client.acquire(&whole_host()).await.unwrap(),
        client.acquire(&whole_host()).await,
    );
    let Acquired::Granted(other) = full.0 else {
        panic!("node2 is free")
    };
    assert_eq!(other.host, "node2");
    assert_eq!(
        refused(full.1.unwrap_err()),
        (409, ErrorCode::NoCapacity),
        "node1 is still busy"
    );

    assert!(server.finish_run(&also_running, kbf_lease_client::Outcome::Killed, None, None));
    assert!(server.end_run(&also_running));
    assert_eq!(
        client.leases().await.unwrap().len(),
        2,
        "one run is still live"
    );
    assert!(server.finish_run(&running, kbf_lease_client::Outcome::Killed, None, None));
    assert!(server.end_run(&running));
    assert!(client.leases().await.unwrap().iter().all(|l| l.id != id));
    assert!(server.run_state(&queued).is_none());
    let gone = client.release(id).await.unwrap_err();
    assert_eq!(refused(gone), (404, ErrorCode::UnknownLease));
}

/// Asserts what the host list shows and what it means for the client: a free host, a leased
/// host with its lease, a lost host (`is_lost`), a tainted one (`is_tainted`), and a host
/// that has not said hello since a restart. Lost and awaiting hosts take no lease. It catches
/// a fake that reports a lost host as live (or the reverse), and a pool that leases
/// a host nobody has heard from.
#[tokio::test]
async fn the_host_list_shows_who_is_lost_tainted_or_leased() {
    let (server, client) = fake().await;
    let hosts = client.hosts().await.unwrap();
    assert_eq!(
        hosts.iter().map(|h| h.id.as_str()).collect::<Vec<_>>(),
        ["node1", "node2"]
    );
    assert!(
        hosts
            .iter()
            .all(|h| h.lease.is_none() && !h.is_lost() && !h.is_tainted())
    );
    assert_eq!(hosts[0].shape.cpu_millis, 16_000);
    assert_eq!(hosts[0].capabilities["os"], "linux");

    let id = lease(&client).await;
    assert!(server.set_lost("node2", true));
    assert!(server.taint_host("node1", "post-job-check-failed"));
    let hosts = client.hosts().await.unwrap();
    assert_eq!(hosts[0].lease, Some(id));
    assert!(hosts[0].is_tainted() && !hosts[0].is_lost());
    assert_eq!(
        hosts[0].taint.as_ref().unwrap().reason,
        "post-job-check-failed"
    );
    assert!(hosts[1].is_lost() && !hosts[1].is_tainted());
    assert_eq!(hosts[1].state, Some(kbf_lease_client::HostState::Lost));
    assert_eq!(hosts[1].lost, Some(true));
    assert_eq!(hosts[0].state, Some(kbf_lease_client::HostState::Live));
    assert_eq!(hosts[0].lost, Some(false));
    let none = client.acquire(&whole_host()).await.unwrap_err();
    assert_eq!(
        refused(none),
        (409, ErrorCode::NoCapacity),
        "a lost host is not leased"
    );

    assert!(server.set_lost("node2", false));
    assert!(!server.set_lost("nowhere", true));
    assert!(!server.taint_host("nowhere", "x"));
    server.restart();
    let hosts = client.hosts().await.unwrap();
    assert!(
        hosts
            .iter()
            .all(|h| h.state == Some(kbf_lease_client::HostState::Awaiting))
    );
    assert_eq!(client.leases().await.unwrap()[0].id, id, "leases persist");
    let none = client.acquire(&whole_host()).await.unwrap_err();
    assert_eq!(
        refused(none),
        (409, ErrorCode::NoCapacity),
        "an awaiting host is not leased"
    );
    assert!(server.host_hello("node2"));
    assert!(!server.host_hello("nowhere"));
    let Acquired::Granted(again) = client.acquire(&whole_host()).await.unwrap() else {
        panic!("node2 said hello");
    };
    assert_eq!(again.host, "node2");
    assert_eq!(again.id.term(), 5, "the term went up with the restart");
}

/// Asserts that a lease can ask for less than a host and still holds the whole host: the
/// server books every dimension. It catches a fake that shares a host between small leases
/// (the executor's arithmetic of one lease per host would be wrong).
#[tokio::test]
async fn a_small_lease_still_books_the_whole_host() {
    let (_server, client) = fake().await;
    let small = Resources {
        cpu_millis: 1000,
        memory_bytes: 1 << 30,
        ..Resources::default()
    };
    let mut request = whole_host();
    request.resources = small;
    client.acquire(&request).await.unwrap();
    client.acquire(&request).await.unwrap();
    let error = client.acquire(&request).await.unwrap_err();
    assert_eq!(refused(error), (409, ErrorCode::NoCapacity));
}

/// Asserts the load the fake lists per host: none until set, the set reading on its own host
/// only, none again once cleared, and none for a lost host (as the server lists), back when
/// it answers again. It catches a load shown on the wrong host, one that outlives its
/// clearing, and a lost host that keeps a reading no consumer should believe.
#[tokio::test]
async fn the_host_list_shows_the_load_a_host_reported() {
    let (server, client) = fake().await;
    let reading = HostLoad {
        seq: 3,
        mono_usec: 9_000_000,
        cpu_usage_usec: 4_000_000,
        memory_working_set_bytes: 1 << 30,
        memory_active_file_bytes: 0,
        actions_cpu_limited: false,
        covered: Vec::new(),
        lease_clocks: Vec::new(),
        minutes: Vec::new(),
    };
    assert!(
        client
            .hosts()
            .await
            .unwrap()
            .iter()
            .all(|h| h.load.is_none())
    );
    assert!(server.set_host_load("node2", Some(reading.clone())));
    assert!(!server.set_host_load("nowhere", Some(reading.clone())));
    let hosts = client.hosts().await.unwrap();
    assert_eq!(hosts[0].load, None);
    assert_eq!(hosts[1].load.as_ref(), Some(&reading));
    assert!(server.set_lost("node2", true));
    assert_eq!(client.hosts().await.unwrap()[1].load, None);
    assert!(server.set_lost("node2", false));
    assert_eq!(
        client.hosts().await.unwrap()[1].load.as_ref(),
        Some(&reading)
    );
    assert!(server.set_host_load("node2", None));
    assert_eq!(client.hosts().await.unwrap()[1].load, None);
}
