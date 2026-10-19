//! Roles of a managed frontend: candidate, holder, quiesce/resume, takeover, stand-down.

use super::*;

/// Catches: a candidate that takes the generation when it opens the store (the holder
/// would be fenced by a process that serves nothing), that writes to the store at all
/// (the commit counter moves), that binds a worker listener (daemons would dial a process
/// that cannot schedule), or that runs the scheduler (an Execute is not refused). It
/// answers `/readyz`, the health service and `GetCapabilities`. Mutant: the candidate's
/// open takes the generation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_candidate_takes_nothing_schedules_nothing_and_answers_readiness() {
    let node = Node::start().await;
    let store = shared();
    let a = Child::start(&managed_flags(&node, &[]), &store).await;
    let (held, version) = generation(&store);
    let commits = store.commit_stats();

    let worker = format!("--worker-listen=127.0.0.1:{}", free_port());
    let flags = managed_flags(&node, &["--await-takeover", &worker]);
    let b = Child::start(&flags, &store).await;
    meanwhile().await;

    assert_eq!(
        generation(&store),
        (held, version),
        "the candidate took the generation"
    );
    assert_eq!(
        store.commit_stats(),
        commits,
        "the candidate wrote to the store"
    );
    let view = b.role().await;
    assert_eq!(view["role"], "candidate");
    assert_eq!(view["generation"], held);
    assert_eq!(view["held_generation"], Value::Null);
    assert!(Child::worker_of(&view).is_none());
    let configured = flags.server.worker_listen;
    assert!(
        refused(configured).await,
        "a candidate has a worker listener"
    );
    until("the candidate is ready", async || {
        readyz(b.api).await == 200
    })
    .await;
    capabilities(b.reapi).await.expect("GetCapabilities");
    let job = Job::new("a candidate does not schedule", &[]);
    let holder = Client::connect(a.reapi, flags.server.worker_listen).await;
    holder.upload(&job.blobs()).await;
    let client = Client::connect(b.reapi, configured).await;
    assert_eq!(
        execute_refused(&client, &job).await,
        Some(Code::Unimplemented),
        "a candidate serves Execute"
    );
    assert_eq!(a.role().await["role"], "active");
}

/// Catches: a `role` that does not say what the process is: a candidate that says active
/// (a proxy would route to a process that schedules nothing), a taken generation not
/// reported by the holder and the store, a quiesced process that says active, a stood
/// down one that says anything else, and an instance id that is not a UUID or does not
/// differ between processes. Mutant: `role` reports active for a candidate.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn role_answers_truthfully_in_each_state() {
    let node = Node::start().await;
    let store = shared();
    let a = Child::start(&managed_flags(&node, &[]), &store).await;
    let b = Child::start(&managed_flags(&node, &["--await-takeover"]), &store).await;

    let view = a.role().await;
    assert_eq!(view["role"], "active");
    assert_eq!(view["admin_protocol"], ADMIN_PROTOCOL);
    assert_eq!(view["instance_id"], a.id);
    let (held, _) = generation(&store);
    assert_eq!(
        (
            view["generation"].as_u64(),
            view["held_generation"].as_u64()
        ),
        (Some(held), Some(held))
    );
    assert!(
        Child::worker_of(&view).is_some(),
        "an active process has a worker listener"
    );
    assert_ne!(a.id, b.id);
    for id in [&a.id, &b.id] {
        let parts: Vec<usize> = id.split('-').map(str::len).collect();
        assert_eq!(parts, [8, 4, 4, 4, 12], "{id}");
        assert_eq!(id.as_bytes()[14], b'4', "{id}");
    }
    assert_eq!(b.role().await["role"], "candidate");

    assert_eq!(a.post("quiesce").await.1["role"], "quiesced");
    assert_eq!(a.role().await["role"], "quiesced");
    let (code, took) = b.takeover(held).await;
    assert_eq!(code, 200, "{took}");
    assert_eq!(took["role"], "active");
    assert_eq!(took["held_generation"], held + 1);
    assert_eq!(b.role().await["generation"], held + 1);
    assert_eq!(
        a.role().await["generation"],
        held + 1,
        "the old holder reports the store's generation"
    );
    assert_eq!(a.role().await["held_generation"], held);

    let (code, stood) = a.post("stand-down").await;
    assert_eq!(code, 200, "{stood}");
    assert_eq!(stood["role"], "stood-down");
    assert_eq!(stood["worker"], Value::Null);
    assert_eq!(a.role().await["role"], "stood-down");
    assert_eq!(b.role().await["role"], "active");
}

/// Catches: a takeover that loses a build in flight. A holds a daemon and a client's
/// Execute stream. After quiesce on A and a takeover on B: B holds the next generation and
/// the epoch is kept (the daemon, ended by A's stand-down, redials B and keeps the lease
/// it held), A's next write is `Fenced` (a cordon through its operator API leaves it
/// superseded), the client's stream, still on A, is answered with the result B took from
/// the daemon, and A refuses a resume (the store is not at its generation). Mutants: the
/// takeover bumps the epoch; the stand-down not ending the daemon's stream.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_takeover_after_a_quiesce_fences_the_old_holder_which_finishes_its_open_calls() {
    let node = Node::start().await;
    let store = shared();
    let scratch = Scratch::new("token");
    let token_flag = scratch.token_flag();
    let a = Child::start(&managed_flags(&node, &[&token_flag]), &store).await;
    let b = Child::start(&managed_flags(&node, &["--await-takeover"]), &store).await;
    let a_worker = Child::worker_of(&a.role().await).expect("a worker listener");
    let client = Client::connect(a.reapi, a_worker).await;
    let mut daemon = client.daemon("node-a", 4, 8).await;
    let job = Job::new("a build across a takeover", &[]);
    client.upload(&job.blobs()).await;
    let mut ops = client.execute(&job.action).await;
    let lease = daemon.start().await.lease_id.expect("a lease id");
    let (held, _) = generation(&store);
    let before = epoch(&store);

    assert_eq!(a.post("quiesce").await.0, 200);
    let (code, took) = b.takeover(held).await;
    assert_eq!(code, 200, "{took}");
    assert_eq!(
        generation(&store).0,
        held + 1,
        "B holds the next generation"
    );
    assert_eq!(epoch(&store), before, "a takeover bumped the epoch");

    // A's next write is refused: a cordon is a write through the fenced handle.
    let cordon = get_post(a.api, "/v1/nodes/node-a:cordon").await;
    assert_eq!(cordon, 200);
    until("A sees it is fenced", async || {
        a.role().await["superseded"] == true
    })
    .await;
    let (code, refused) = a.post("resume").await;
    assert_eq!(code, 409, "{refused}");
    assert_eq!(a.role().await["role"], "quiesced");

    assert_eq!(a.post("stand-down").await.0, 200);
    let ended = daemon.ended().await;
    assert_eq!(ended.code(), Code::Unavailable, "{ended}");
    let reached =
        Client::connect(b.reapi, Child::worker_of(&b.role().await).expect("worker")).await;
    let mut daemon = reached.daemon("node-a", 4, 8).await;
    assert_eq!(daemon.epoch, lease.term, "the epoch was kept");
    assert!(daemon.heartbeat(&[lease]).await);
    assert!(
        !done_within_quiet(&mut ops).await,
        "answered before the result"
    );
    let result = support::output(&reached, "built across a takeover", 0).await;
    assert!(
        daemon.report(ran(Some(lease), &result)).await.accepted,
        "B did not take the old lease"
    );
    let last = done(&mut ops).await;
    assert_eq!(response(&last).result, Some(result));
}

/// Catches: a quiesce that does not stop scheduling (an Execute is accepted and the daemon
/// is handed work), one that cannot be undone (after a takeover that did not happen the
/// holder would never schedule again), and a resume that does not bring back what the
/// store holds. A quiesced A refuses an Execute and assigns nothing; after `resume` the
/// same action is queued and the daemon, connected the whole time, starts it. Mutants:
/// quiesce does nothing; resume does not clear the fence.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn quiesce_then_resume_restores_scheduling_from_the_store() {
    let node = Node::start().await;
    let store = shared();
    let a = Child::start(&managed_flags(&node, &[]), &store).await;
    let a_worker = Child::worker_of(&a.role().await).expect("a worker listener");
    let client = Client::connect(a.reapi, a_worker).await;
    let mut daemon = client.daemon("node-a", 4, 8).await;
    let job = Job::new("work that waits for a resume", &[]);
    client.upload(&job.blobs()).await;
    let (held, _) = generation(&store);

    let (code, view) = a.post("quiesce").await;
    assert_eq!((code, view["role"].as_str()), (200, Some("quiesced")));
    assert_eq!(a.post("quiesce").await.0, 200, "quiesce is idempotent");
    assert!(
        execute_refused(&client, &job).await.is_some(),
        "a quiesced process queued work"
    );
    daemon.no_work().await;
    assert_eq!(generation(&store).0, held, "quiesce moved the generation");

    let (code, view) = a.post("resume").await;
    assert_eq!(
        (code, view["role"].as_str()),
        (200, Some("active")),
        "{view}"
    );
    let mut ops = client.execute(&job.action).await;
    let lease = daemon.start().await.lease_id.expect("a lease id");
    let result = support::output(&client, "work that waits for a resume", 0).await;
    assert!(daemon.report(ran(Some(lease), &result)).await.accepted);
    assert_eq!(response(&done(&mut ops).await).result, Some(result));
}

/// Catches: a stand-down that leaves daemons attached (their streams are not ended), that
/// leaves the worker listener accepting (a daemon that redials reaches a process that
/// schedules nothing), that stays ready (a proxy would keep choosing it), or that cuts the
/// REAPI listener (open Execute and WaitExecution streams would end). A candidate or an
/// unknown call is refused. Mutant: the stand-down leaves the worker listener up.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stand_down_ends_worker_streams_and_leaves_no_worker_listener() {
    let node = Node::start().await;
    let store = shared();
    let a = Child::start(&managed_flags(&node, &[]), &store).await;
    let a_worker = Child::worker_of(&a.role().await).expect("a worker listener");
    let client = Client::connect(a.reapi, a_worker).await;
    let mut daemon = client.daemon("node-a", 4, 8).await;
    assert_eq!(readyz(a.api).await, 200);

    let (code, view) = a.post("stand-down").await;
    assert_eq!(
        (code, view["role"].as_str()),
        (200, Some("stood-down")),
        "{view}"
    );
    let ended = daemon.ended().await;
    assert_eq!(ended.code(), Code::Unavailable, "{ended}");
    until("the worker listener is gone", async || {
        refused(a_worker).await
    })
    .await;
    until("the process is not ready", async || {
        readyz(a.api).await == 503
    })
    .await;
    assert!(!refused(a.reapi).await, "the REAPI listener was cut");
    assert_eq!(
        a.post("stand-down").await.0,
        200,
        "stand-down is idempotent"
    );
    assert_eq!(a.post("quiesce").await.0, 409);
    assert_eq!(a.post("resume").await.0, 409);
}

/// Catches: an admin port reachable from off the host (the calls move the scheduler's role
/// and carry no credential): a non-loopback `--admin-listen` is refused at parse, and a
/// start given one anyway is refused, so is `--await-takeover` without the port, with a
/// handover socket, or a port without a state store. Loopback v4 and v6 are accepted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_admin_port_refuses_a_non_loopback_address() {
    for bad in [
        "0.0.0.0:7001",
        "[::]:7001",
        "192.0.2.7:7001",
        "[2001:db8::1]:7001",
    ] {
        let refused = flags(&[
            "--storage=http://127.0.0.1:1",
            "--state-store=sqlite:///x.db",
            &format!("--admin-listen={bad}"),
        ])
        .expect_err(bad);
        assert!(refused.to_string().contains("loopback"), "{bad}: {refused}");
    }
    for good in ["127.0.0.1:7001", "[::1]:7001"] {
        let parsed = flags(&[
            "--storage=http://127.0.0.1:1",
            "--state-store=sqlite:///x.db",
            &format!("--admin-listen={good}"),
        ])
        .expect(good);
        assert_eq!(
            parsed.admin.expect("admin flags").listen,
            good.parse().unwrap()
        );
    }
    assert!(
        flags(&[
            "--storage=http://127.0.0.1:1",
            "--state-store=sqlite:///x.db",
            "--await-takeover"
        ])
        .is_err()
    );
    assert!(
        flags(&["--storage=http://127.0.0.1:1", "--admin-listen=127.0.0.1:0"]).is_err(),
        "no state store"
    );
    assert!(
        flags(&[
            "--storage=http://127.0.0.1:1",
            "--state-store=sqlite:///x.db",
            "--admin-listen=127.0.0.1:0",
            "--await-takeover",
            "--handover-socket=/run/kbf/c.sock",
        ])
        .is_err()
    );

    let node = Node::start().await;
    let mut open = managed_flags(&node, &[]);
    open.admin.as_mut().expect("admin flags").listen = "0.0.0.0:0".parse().unwrap();
    let error = Managed::start(
        &open,
        StoreSource::Shared(shared()),
        std::future::pending::<()>(),
    )
    .await
    .err()
    .expect("a start on a public address is refused");
    assert!(error.to_string().contains("loopback"), "{error}");

    // A start with no admin port at all is refused before anything is bound or opened.
    let mut none = managed_flags(&node, &[]);
    none.admin = None;
    let error = Managed::start(
        &none,
        StoreSource::Shared(shared()),
        std::future::pending::<()>(),
    )
    .await
    .err()
    .expect("a start without --admin-listen is refused");
    assert!(error.to_string().contains("--admin-listen"), "{error}");
}

/// Catches: an epoch that does not follow the rule. A takeover from a live holder keeps
/// it (daemons keep their leases), a candidate's open leaves it as it was, and a cold
/// start over the same store bumps it (a start with no live holder cannot trust what the
/// last one leased). Mutant: the takeover bumps the epoch.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_epoch_is_kept_by_a_takeover_and_bumped_by_a_cold_start() {
    let node = Node::start().await;
    let store = shared();
    let a = Child::start(&managed_flags(&node, &[]), &store).await;
    let first = epoch(&store);
    let b = Child::start(&managed_flags(&node, &["--await-takeover"]), &store).await;
    assert_eq!(epoch(&store), first, "a candidate's open moved the epoch");
    let (held, _) = generation(&store);
    assert_eq!(a.post("quiesce").await.0, 200);
    assert_eq!(b.takeover(held).await.0, 200);
    assert_eq!(epoch(&store), first, "a takeover moved the epoch");
    let c = Child::start(&managed_flags(&node, &[]), &store).await;
    assert_eq!(epoch(&store), first + 1, "a cold start kept the epoch");
    assert_eq!(c.role().await["held_generation"], held + 2);
}
