//! Managed frontend lifecycle: refused calls, fencing, stopping, store failures, flags.

use super::*;

/// Catches: a takeover that writes when the holder moved on (the candidate would fence a
/// holder it was not told about), one that answers a body it does not understand, and
/// calls that change a role they do not apply to. A wrong `expect` leaves the generation
/// and the candidate as they were and names the generation found.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn calls_that_do_not_apply_change_nothing() {
    let node = Node::start().await;
    let store = shared();
    let a = Child::start(&managed_flags(&node, &[]), &store).await;
    let b = Child::start(&managed_flags(&node, &["--await-takeover"]), &store).await;
    let (held, version) = generation(&store);

    let (code, answer) = b.takeover(held + 5).await;
    assert_eq!(code, 409, "{answer}");
    assert!(
        answer["error"]
            .as_str()
            .is_some_and(|e| e.contains(&held.to_string())),
        "{answer}"
    );
    assert_eq!(answer["role"], "candidate");
    assert_eq!(
        generation(&store),
        (held, version),
        "a refused takeover wrote"
    );
    assert_eq!(b.call("POST", "/v1/admin/takeover", "{}").await.0, 400);
    assert_eq!(
        b.call("POST", "/v1/admin/takeover", "{\"expect\": 1, \"more\": 2}")
            .await
            .0,
        400
    );
    for verb in ["quiesce", "resume", "stand-down"] {
        assert_eq!(b.post(verb).await.0, 409, "{verb} on a candidate");
    }
    assert_eq!(
        a.takeover(held).await.0,
        409,
        "takeover on an active process"
    );
    let (code, view) = a.post("resume").await;
    assert_eq!(
        (code, view["role"].as_str()),
        (200, Some("active")),
        "a resume of a process that never quiesced is a no-op"
    );
    assert_eq!(a.call("GET", "/v1/admin/nothing", "").await.0, 404);
    assert_eq!(a.role().await["role"], "active");
    assert_eq!(b.role().await["role"], "candidate");
}

/// Catches: an old holder that is fenced by a takeover and is never told to stand down
/// (the supervisor died, or the call was lost) leaving its callers hanging: a caller that
/// waits on Execute at A is answered once B writes the `done/` row, though A was only
/// found fenced by a refused write of its own (a cordon). Mutant: nothing follows the
/// `done/` rows of a process found superseded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fenced_holder_answers_its_waiting_callers_without_a_stand_down() {
    let node = Node::start().await;
    let store = shared();
    let scratch = Scratch::new("fenced");
    let token_flag = scratch.token_flag();
    let a = Child::start(&managed_flags(&node, &[&token_flag]), &store).await;
    let b = Child::start(&managed_flags(&node, &["--await-takeover"]), &store).await;
    let a_worker = Child::worker_of(&a.role().await).expect("a worker listener");
    let client = Client::connect(a.reapi, a_worker).await;
    let mut daemon = client.daemon("node-a", 4, 8).await;
    let job = Job::new("a build whose holder is fenced", &[]);
    client.upload(&job.blobs()).await;
    let mut ops = client.execute(&job.action).await;
    let lease = daemon.start().await.lease_id.expect("a lease id");
    let (held, _) = generation(&store);

    // B takes the generation with no quiesce and no stand-down on A.
    assert_eq!(b.takeover(held).await.0, 200);
    assert_eq!(a.role().await["superseded"], false, "A has not written yet");
    assert_eq!(get_post(a.api, "/v1/nodes/node-a:cordon").await, 200);
    until("A finds it is fenced", async || {
        a.role().await["superseded"] == true
    })
    .await;
    assert_eq!(a.role().await["role"], "active", "no stand-down was made");
    // A found it is fenced and fenced its farm: a call that would queue is refused
    // UNAVAILABLE ("handing over"), not as the store's refusal of a write
    // (FAILED_PRECONDITION), which is what an unfenced farm that knows only its store is
    // superseded answers.
    let other = Job::new("work for a fenced holder", &[]);
    client.upload(&other.blobs()).await;
    until("the farm of a holder found fenced is fenced", async || {
        execute_refused(&client, &other).await == Some(Code::Unavailable)
    })
    .await;

    let b_worker = Child::worker_of(&b.role().await).expect("worker");
    let reached = Client::connect(b.reapi, b_worker).await;
    let mut daemon = reached.daemon("node-a", 4, 8).await;
    assert!(daemon.heartbeat(&[lease]).await);
    let result = support::output(&reached, "a build whose holder is fenced", 0).await;
    assert!(daemon.report(ran(Some(lease), &result)).await.accepted);
    let last = done(&mut ops).await;
    assert_eq!(response(&last).result, Some(result));
}

/// A process that can be told to stop, and whose end can be read.
pub(super) struct Stoppable {
    child: Child,
    stop: oneshot::Sender<()>,
    ended: JoinHandle<Result<(), kbf_server::managed::ManagedError>>,
    line: String,
}

impl Stoppable {
    pub(super) async fn start(flags: &FrontendFlags, source: StoreSource) -> Self {
        let (stop, stopped) = oneshot::channel::<()>();
        let managed = Managed::start(flags, source, async move {
            let _ = stopped.await;
        })
        .await
        .expect("starts")
        .escalating(std::future::pending::<()>())
        .notifying(None);
        let view = managed.view();
        let line = managed.start_line();
        let child = Child {
            admin: managed.admin(),
            reapi: view.reapi.expect("a REAPI listener"),
            api: view.api.expect("an operator API"),
            id: view.instance_id,
        };
        Self {
            child,
            stop,
            ended: tokio::spawn(managed.serve()),
            line,
        }
    }

    /// Stops it and returns how its serving ended.
    pub(super) async fn stopped(self) -> Result<(), String> {
        let _ = self.stop.send(());
        tokio::time::timeout(Duration::from_secs(20), self.ended)
            .await
            .expect("it ends")
            .expect("the task")
            .map_err(|error| error.to_string())
    }
}

/// Catches: a managed process that does not end when told to stop (a candidate waiting
/// for a takeover that never comes would hold its ports for ever; an active one would
/// ignore the signal), and a start line that does not name the admin port, the instance,
/// the role and the protocol a supervisor reads it for.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_managed_process_ends_when_told_to_stop() {
    let node = Node::start().await;
    let store = shared();
    let source = || StoreSource::Shared(Arc::clone(&store) as Arc<dyn Store>);
    let active = Stoppable::start(&managed_flags(&node, &[]), source()).await;
    let candidate = Stoppable::start(&managed_flags(&node, &["--await-takeover"]), source()).await;
    for (process, role) in [(&active, "active"), (&candidate, "candidate")] {
        let line = &process.line;
        assert!(line.starts_with("kbf-frontend-admin admin="), "{line}");
        assert!(
            line.contains(&format!("admin={}", process.child.admin)),
            "{line}"
        );
        assert!(
            line.contains(&format!("instance={}", process.child.id)),
            "{line}"
        );
        assert!(
            line.contains(&format!("role={role} admin_protocol={ADMIN_PROTOCOL}")),
            "{line}"
        );
    }
    assert_eq!(candidate.stopped().await, Ok(()));
    assert_eq!(active.stopped().await, Ok(()));
}

/// Catches: a takeover that took the generation and could not start the frontend, which
/// the process hides (the supervisor would believe a candidate that holds the generation
/// and serves nothing): the call answers 500 with the reason and the process ends with it.
/// Here the worker address is taken by someone else.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_takeover_that_cannot_start_ends_the_process_with_the_reason() {
    let node = Node::start().await;
    let store = shared();
    let a = Child::start(&managed_flags(&node, &[]), &store).await;
    let taken = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let worker = format!("--worker-listen={}", taken.local_addr().expect("addr"));
    let b = Stoppable::start(
        &managed_flags(&node, &["--await-takeover", &worker]),
        StoreSource::Shared(Arc::clone(&store) as Arc<dyn Store>),
    )
    .await;
    let (held, _) = generation(&store);
    assert_eq!(a.post("quiesce").await.0, 200);
    let (code, answer) = b.child.takeover(held).await;
    assert_eq!(code, 500, "{answer}");
    assert!(
        answer["error"].as_str().is_some_and(|e| e.contains("bind")),
        "{answer}"
    );
    assert_eq!(generation(&store).0, held + 1, "the generation was taken");
    let ended = b
        .stopped()
        .await
        .expect_err("the process ends with the reason");
    assert!(ended.contains("bind"), "{ended}");
}

/// A store whose reads can be made to fail.
struct Flaky {
    inner: Arc<Fake>,
    failing: Arc<std::sync::atomic::AtomicBool>,
}

impl Flaky {
    fn check(&self) -> Result<(), StoreError> {
        if self.failing.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(StoreError::Failed("planted".to_owned()));
        }
        Ok(())
    }
}

impl Store for Flaky {
    fn apply_batch(
        &self,
        batch: Vec<Txn>,
    ) -> Result<Vec<Result<kbf_state::Commit, Conflict>>, StoreError> {
        self.inner.apply_batch(batch)
    }
    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        self.check()?;
        self.inner.get(key)
    }
    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        self.check()?;
        self.inner.scan_prefix(prefix)
    }
    fn scan_range(&self, scan: ScanRange<'_>) -> Result<ScanPage, StoreError> {
        self.inner.scan_range(scan)
    }
    fn changes(&self, after: Revision, limit: usize) -> Result<Changes, StoreError> {
        self.inner.changes(after, limit)
    }
    fn reap_expired(&self) -> Result<usize, StoreError> {
        self.inner.reap_expired()
    }
    fn commit_stats(&self) -> CommitStats {
        self.inner.commit_stats()
    }
    fn now(&self) -> StoreTime {
        self.inner.now()
    }
}

/// Catches: a resume that guesses when the store cannot be read (it must not put a
/// process back to work on a generation it cannot confirm: 500, still quiesced), and a
/// `role` that invents a generation (it says `null`); once the store answers again the
/// resume works.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_store_that_cannot_be_read_is_reported_and_stops_a_resume() {
    let node = Node::start().await;
    let failing = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flaky: Arc<dyn Store> = Arc::new(Flaky {
        inner: shared(),
        failing: Arc::clone(&failing),
    });
    let managed = Managed::start(
        &managed_flags(&node, &[]),
        StoreSource::Shared(flaky),
        std::future::pending::<()>(),
    )
    .await
    .expect("starts");
    let a = Child {
        admin: managed.admin(),
        reapi: managed.view().reapi.expect("reapi"),
        api: managed.view().api.expect("api"),
        id: managed.view().instance_id,
    };
    tokio::spawn(managed.serve());
    assert_eq!(a.post("quiesce").await.0, 200);
    failing.store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(a.role().await["generation"], Value::Null);
    let (code, answer) = a.post("resume").await;
    assert_eq!(code, 500, "{answer}");
    assert_eq!(answer["role"], "quiesced");
    failing.store(false, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(a.post("resume").await.0, 200);
}

/// Catches: a candidate that starts with an operator API address someone else holds
/// (the start must fail with the address, not run with half its listeners).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_candidate_whose_listener_cannot_be_bound_does_not_start() {
    let node = Node::start().await;
    let taken = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let api = format!("--api-listen={}", taken.local_addr().expect("addr"));
    let flags = managed_flags(&node, &["--await-takeover", &api]);
    let error = Managed::start(
        &flags,
        StoreSource::Shared(shared()),
        std::future::pending::<()>(),
    )
    .await
    .err()
    .expect("a bound address is refused");
    assert!(error.to_string().contains("bind"), "{error}");
}

/// Catches: a takeover that waits for ever on a probe's open health `Watch` stream (the
/// stand-in's graceful shutdown never finishes while it is open): the stand-in goes not
/// ready, the stream ends, and the takeover finishes at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_open_health_watch_does_not_hold_up_a_takeover() {
    use kbf_proto::grpc::health::v1::health_client::HealthClient;
    let node = Node::start().await;
    let store = shared();
    let a = Child::start(&managed_flags(&node, &[]), &store).await;
    let b = Child::start(&managed_flags(&node, &["--await-takeover"]), &store).await;
    let channel = Endpoint::from_shared(format!("http://{}", b.reapi))
        .expect("uri")
        .connect()
        .await
        .expect("connect");
    let mut watch = HealthClient::new(channel)
        .watch(HealthCheckRequest::default())
        .await
        .expect("watch")
        .into_inner();
    watch.message().await.expect("first").expect("a status");
    let (held, _) = generation(&store);
    assert_eq!(a.post("quiesce").await.0, 200);
    let started = std::time::Instant::now();
    assert_eq!(b.takeover(held).await.0, 200);
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "{:?}",
        started.elapsed()
    );
    let end = tokio::time::timeout(Duration::from_secs(5), async {
        while let Ok(Some(_)) = watch.message().await {}
    })
    .await;
    assert!(end.is_ok(), "the watch on the stand-in was left open");
}

/// Catches: a process that cannot open the state store the flags name: a cold start over
/// a SQLite file takes its first generation; a candidate over another file opens it and
/// takes none.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_managed_process_opens_the_store_its_flags_name() {
    let node = Node::start().await;
    let dir = std::env::temp_dir().join(format!("kbf-managed-flags-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let state = |name: &str| format!("--state-store=sqlite://{}/{name}.db", dir.display());
    let cold = Stoppable::start(&managed_flags(&node, &[&state("cold")]), StoreSource::Flags).await;
    let view = cold.child.role().await;
    assert_eq!(
        (view["role"].as_str(), view["held_generation"].as_u64()),
        (Some("active"), Some(1))
    );
    let waiting = Stoppable::start(
        &managed_flags(&node, &["--await-takeover", &state("waiting")]),
        StoreSource::Flags,
    )
    .await;
    let view = waiting.child.role().await;
    assert_eq!(
        (view["role"].as_str(), view["generation"].as_u64()),
        (Some("candidate"), Some(0))
    );
    assert_eq!(waiting.stopped().await, Ok(()));
    assert_eq!(cold.stopped().await, Ok(()));

    // The same cold start with a shutdown that never comes: the instance of `start` the
    // other tests use for it (`std::future::pending`) must itself run the flags path, or
    // coverage of the function is split between two instances.
    let never = Managed::start(
        &managed_flags(&node, &[&state("never")]),
        StoreSource::Flags,
        std::future::pending::<()>(),
    )
    .await
    .expect("a cold start over a file");
    assert!(
        never.start_line().contains("role=active"),
        "{}",
        never.start_line()
    );
    drop(never);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Catches: a candidate that needs an operator API (a REAPI-only deployment would not
/// start), a start from the flags with no state store that panics or starts over nothing
/// (it is refused naming the flag).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_candidate_serves_without_an_operator_api_and_the_flags_need_a_store() {
    let node = Node::start().await;
    let mut flags = managed_flags(&node, &["--await-takeover"]);
    flags.server.api_listen = None;
    let managed = Managed::start(
        &flags,
        StoreSource::Shared(shared()),
        std::future::pending::<()>(),
    )
    .await
    .expect("starts with no operator API");
    let view = managed.view();
    assert!(view.api.is_none());
    capabilities(view.reapi.expect("a REAPI listener"))
        .await
        .expect("GetCapabilities");

    let mut none = managed_flags(&node, &["--await-takeover"]);
    none.server.state_store = None;
    let error = Managed::start(&none, StoreSource::Flags, std::future::pending::<()>())
        .await
        .err()
        .expect("no store to open");
    assert!(error.to_string().contains("--state-store"), "{error}");
}
