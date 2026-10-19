//! An all-in-one frontend swap with 200 streams held across it (docs/design/frontend-proxy.md,
//! FP section 5a, and the U7b proof of its unit table: "an all-in-one swap with 200 held
//! streams fed to the new child").
//!
//! The swap is the installer's own (`kbf_installer::proxy::Swap`, the all-in-one flow of
//! `aio.rs`) over the proxy's real gate (`ProxyGate` on the `ProxyHandle` of a real
//! `kbf-proxy`, TLS and all). Its process side is real too: the children are `kbf frontend`
//! processes (this test binary run through a link named `kbf`, as `proxy_soak.rs` does), in
//! front of one `kbf storage` node that both releases share, as an all-in-one host's two
//! releases share the state they leave behind. Slot A runs the old release and slot B the new
//! one. The swap records its `update.json` through the real `FileRecorder`, and sleeps on the
//! real `SystemClock` with the all-in-one timing (`hold_max` is `ALL_IN_ONE_HOLD`, 300 s).
//!
//! What is a stand-in, said once: a real all-in-one process needs a container runtime and a
//! state store and is not what the swap's order is about, so each child here is a frontend
//! (the part the proxy serves); and `ChildAdmin` is this file's (the real `Admin` answers
//! `Readiness::all()` false until the health and capability probes exist, so no swap built
//! on it gets past the start): `quiesce` is accepted and the old child then reads
//! `Quiesced`, `ready` is the child's own `GET /readyz` (200) once the test opens a latch,
//! and the migration is a `Migrator` that waits for the test. The `Child` port is this file's
//! on `kbf_testkit::reaped`, so no child outlives a failed test (`ProcChild` is the unit
//! tests' subject, `child_real_tests.rs`).
//!
//! What the test does. It stops the swap exactly where the all-in-one hold matters: in the
//! migration, which the swap calls after it has stopped the old process and cleared the
//! active slot, and before it starts the new one. There 200 `Execute` streams arrive through
//! the proxy and are held. Then the swap goes on: the new child is started and is really
//! ready (its own `/readyz` is 200) while the swap's readiness probe is still shut, and the
//! streams must still be held; the probe is opened, the swap releases, and the 200 streams
//! are served.
//!
//! What this proves, exactly:
//! - the old process is gone (it exited 0 after SIGTERM, its port refuses connections) and no
//!   slot is active when the first stream arrives, so a stream the proxy bound there would
//!   have nothing to dial;
//! - exactly 200 streams are held (`held`, the proxy's own count), none is let go while the
//!   new child is up but the swap has not seen it ready, and the gate's `release_into` finds
//!   exactly 200 held and releases them into slot B;
//! - exactly 200 `Execute` streams are then open on B, by B's own `open_executions` of
//!   `GET /v1/frontend/drain`, and the proxy's per-slot gauge says B 200, A 0; none was
//!   dropped, answered `UNAVAILABLE`, expired, refused, retried or failed to dial (every
//!   such counter of `/metrics` is 0, and each stream's own answer is a served first
//!   `Operation`, not a `grpc-status`);
//! - the hold the swap took was `ALL_IN_ONE_HOLD` and ended in less than that;
//! - the proxy's `/readyz` is 200 for B;
//! - the streams end by the client cancelling them and B's count falls to 0 (the streams
//!   complete on B, not elsewhere);
//! - the swap ends `Done` with nothing noted, and A exited 0.
//!
//! A 300 s hold is not waited for: the hold's bound is read from the hold the gate was given
//! and its elapsed time is measured, and a hold that expired early would fail streams, which
//! the exact counts catch.

#![cfg(target_os = "linux")]

mod proxy_support;

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read as _, Write as _};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use kbf_installer::CommitId;
use kbf_installer::proxy::gate_real::ProxyGate;
use kbf_installer::proxy::model::{
    AdoptError, Halt, Locator, Mode, OpenCounts, Phase, Plan, PortError, ProcIdent, Readiness,
    Retired, RoleInfo, RoleState, Slot, StagedFacts, Start, Timing,
};
use kbf_installer::proxy::ports::{Child, ChildAdmin, Clock, Gate, Migrator, Ports, RootOrders};
use kbf_installer::proxy::runtime::{RequestQueue, SystemClock};
use kbf_installer::proxy::{FileRecorder, Swap};
use kbf_proxy::Proxy;
use kbf_proxy::control::NoUpdates;
use kbf_proxy::hold::ALL_IN_ONE_HOLD;
use kbf_proxy::slot::Slot as GateSlot;
use kbf_testkit::reaped::{self, Reaped};
use proxy_support::{
    Open, actions, api_get, connect, free_base, frontend_args, open_executions, proxy_config,
    proxy_get, storage_node, store, until, until_ready, within,
};

/// Streams that arrive while the swap holds, between the old process's stop and the new
/// process's start.
const HELD: usize = 200;

/// How long the streams are held with the new child up and the swap not yet ready to release.
const DWELL: Duration = Duration::from_millis(600);

/// The test's clock for its own waits.
const WAIT: Duration = Duration::from_secs(30);

/// The most the whole swap may take. A swap stuck in a wait that has no timeout of its own
/// (the old slot's close, say) is stopped at this bound and the test fails.
const SWAP_BOUND: Duration = Duration::from_secs(120);

/// The system clock whose `sleep` fails once `stop` is set, so a swap stuck in a poll loop
/// returns `Halt::Interrupted` instead of holding `thread::scope` open for ever.
struct Stoppable {
    inner: SystemClock,
    stop: AtomicBool,
}

impl Clock for Stoppable {
    fn now(&self) -> Duration {
        self.inner.now()
    }

    fn sleep(&self, d: Duration) -> Result<(), Halt> {
        if self.stop.load(Ordering::SeqCst) {
            return Err(Halt::Interrupted);
        }
        self.inner.sleep(d.min(Duration::from_millis(50)))
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What the ports did, in order.
#[derive(Default)]
struct Log(Mutex<Vec<String>>);

impl Log {
    fn push(&self, line: impl Into<String>) {
        lock(&self.0).push(line.into());
    }

    fn lines(&self) -> Vec<String> {
        lock(&self.0).clone()
    }
}

/// Waits (at most [`WAIT`]) until `done` holds.
fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(started.elapsed() < WAIT, "gave up waiting: {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Whether `api`'s `GET /readyz` answers 200. A child that is not listening is not ready.
fn readyz(api: &str) -> bool {
    let Ok(mut stream) = TcpStream::connect(api) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    let sent = write!(
        stream,
        "GET /readyz HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\n\r\n"
    );
    let mut text = String::new();
    sent.is_ok() && stream.read_to_string(&mut text).is_ok() && text.starts_with("HTTP/1.1 200")
}

/// The slot's first port: slot A's `base`, slot B's ten above, as the proxy's table has them.
fn slot_base(base: u16, slot: Slot) -> u16 {
    match slot {
        Slot::A => base,
        Slot::B => base + 10,
    }
}

fn operator_api(base: u16, slot: Slot) -> String {
    format!("127.0.0.1:{}", slot_base(base, slot) + 2)
}

/// The `Child` port on real `kbf frontend` processes, each reaped when this is dropped.
struct Procs {
    dir: PathBuf,
    storage: String,
    base: u16,
    log: Arc<Log>,
    running: Mutex<BTreeMap<u32, Reaped>>,
    /// The exit code of each process the swap saw gone, by pid.
    exits: Mutex<BTreeMap<u32, i32>>,
}

impl Child for Procs {
    fn spawn(&self, slot: Slot, _release: &CommitId, start: Start) -> Result<ProcIdent, PortError> {
        assert_eq!(start, Start::Active, "an all-in-one host starts to serve");
        let name = format!("frontend-{slot:?}");
        let out = std::fs::File::create(self.dir.join(format!("{name}.log")))
            .map_err(|e| PortError(e.to_string()))?;
        let mut command = Command::new(self.dir.join("bin/kbf"));
        command
            .args(frontend_args(&self.storage, slot_base(self.base, slot)))
            .stdout(Stdio::from(
                out.try_clone().map_err(|e| PortError(e.to_string()))?,
            ))
            .stderr(out);
        let process = reaped::spawn(&mut command, 3).map_err(|e| PortError(e.to_string()))?;
        let pid = process.id();
        lock(&self.running).insert(pid, process);
        self.log.push(format!("spawn {slot:?}"));
        Ok(ProcIdent {
            pid,
            start_time: 0,
            exe: self.dir.join("bin/kbf"),
        })
    }

    fn adopt(&self, _slot: Slot, recorded: &ProcIdent) -> Result<(), AdoptError> {
        Err(AdoptError::Gone(recorded.pid))
    }

    fn exited(&self, ident: &ProcIdent) -> Option<i32> {
        let mut running = lock(&self.running);
        let status = running.get_mut(&ident.pid)?.try_wait().ok()??;
        let code = status.code().unwrap_or(-1);
        lock(&self.exits).insert(ident.pid, code);
        Some(code)
    }

    fn terminate(&self, ident: &ProcIdent) -> Result<(), PortError> {
        self.log.push(format!("terminate pid {}", ident.pid));
        let pid = rustix::process::Pid::from_raw(ident.pid.cast_signed()).expect("pid");
        rustix::process::kill_process(pid, rustix::process::Signal::TERM)
            .map_err(|e| PortError(e.to_string()))
    }

    fn kill_unserved(&self, ident: &ProcIdent) -> Result<(), PortError> {
        self.log.push(format!("kill pid {}", ident.pid));
        let pid = rustix::process::Pid::from_raw(ident.pid.cast_signed()).expect("pid");
        rustix::process::kill_process(pid, rustix::process::Signal::KILL)
            .map_err(|e| PortError(e.to_string()))
    }
}

/// The admin port of the stand-in children (see the header).
struct Stand {
    base: u16,
    log: Arc<Log>,
    /// The old child has been asked to quiesce.
    quiesced: AtomicBool,
    /// The swap's readiness probe of the new child may pass.
    ready_open: AtomicBool,
    /// How often the swap asked whether the new child is ready, and was told no.
    refused_ready: AtomicUsize,
    /// The old child's own open executions when it was asked to quiesce.
    old_open: Mutex<Option<u64>>,
}

impl ChildAdmin for Stand {
    fn role(&self, slot: Slot) -> Result<RoleInfo, PortError> {
        let quiesced = slot == Slot::A && self.quiesced.load(Ordering::SeqCst);
        Ok(RoleInfo {
            holder: true,
            state: if quiesced {
                RoleState::Quiesced
            } else {
                RoleState::Active
            },
        })
    }

    fn takeover(&self, _: Slot, _: Duration) -> Result<(), PortError> {
        Err(PortError("an all-in-one swap sends no takeover".into()))
    }

    fn quiesce(&self, slot: Slot) -> Result<(), PortError> {
        self.log.push(format!("quiesce {slot:?}"));
        let (code, body) = api_get(&operator_api(self.base, slot), "/v1/frontend/drain");
        assert_eq!(code, 200, "{body}");
        let status: serde_json::Value = serde_json::from_str(&body).expect("JSON");
        *lock(&self.old_open) = status["open_executions"].as_u64();
        self.quiesced.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn stand_down(&self, _: Slot) -> Result<(), PortError> {
        Err(PortError("an all-in-one swap sends no stand-down".into()))
    }

    fn resume(&self, _: Slot) -> Result<(), PortError> {
        Err(PortError("this swap does not abort".into()))
    }

    fn ready(&self, slot: Slot) -> Result<Readiness, PortError> {
        if slot == Slot::B && !self.ready_open.load(Ordering::SeqCst) {
            self.refused_ready.fetch_add(1, Ordering::SeqCst);
            return Ok(Readiness {
                readyz: false,
                health: false,
                capabilities: false,
            });
        }
        let up = readyz(&operator_api(self.base, slot));
        if slot == Slot::B && up {
            self.log.push("ready B");
        }
        Ok(Readiness {
            readyz: up,
            health: up,
            capabilities: up,
        })
    }

    fn open_counts(&self, slot: Slot) -> Result<OpenCounts, PortError> {
        Ok(OpenCounts {
            open_streams: open_executions(&operator_api(self.base, slot)),
            open_sessions: 0,
        })
    }

    fn post_desired(&self, _: Slot, _: &CommitId) -> Result<(), PortError> {
        Err(PortError("the plan updates no nodes".into()))
    }
}

/// The migration: it runs after the old process is stopped, and waits for the test.
struct Migration {
    log: Arc<Log>,
    reached: AtomicBool,
    go: AtomicBool,
}

impl Migrator for Migration {
    fn migrate_forward(&self, _: &CommitId) -> Result<(), PortError> {
        self.log.push("migrate");
        self.reached.store(true, Ordering::SeqCst);
        let started = Instant::now();
        while !self.go.load(Ordering::SeqCst) {
            if started.elapsed() > Duration::from_secs(60) {
                return Err(PortError("the test never let the migration go".into()));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(())
    }
}

/// The root half: the release is staged and a confirm lands.
struct Root {
    to: CommitId,
    log: Arc<Log>,
}

impl RootOrders for Root {
    fn frontend_update(&self, _: &Locator) -> Result<(), PortError> {
        Ok(())
    }

    fn staged(&self) -> Result<Option<StagedFacts>, PortError> {
        Ok(Some(StagedFacts {
            commit: self.to.clone(),
            protocol: 1,
            min_protocol: 1,
            state_schema: 1,
            profile: None,
        }))
    }

    fn installed(&self) -> Result<Option<CommitId>, PortError> {
        Ok(Some(self.to.clone()))
    }

    fn confirm(&self, _: &CommitId) -> Result<(), PortError> {
        self.log.push("confirm");
        Ok(())
    }

    fn revert(&self, _: &CommitId) -> Result<(), PortError> {
        self.log.push("revert");
        Ok(())
    }
}

/// The real gate, and what it was asked, in the order and at the times it was asked.
struct Spy {
    gate: ProxyGate,
    handle: kbf_proxy::ProxyHandle,
    log: Arc<Log>,
    /// The bound of each hold taken, and when.
    holds: Mutex<Vec<(Duration, Instant)>>,
    /// Each release: the slot, the streams the proxy held at that moment, and when.
    releases: Mutex<Vec<(Slot, usize, Instant)>>,
    expired: AtomicUsize,
}

impl Gate for Spy {
    fn hold(&self, max: Duration) -> Result<(), PortError> {
        self.log.push("hold");
        lock(&self.holds).push((max, Instant::now()));
        self.gate.hold(max)
    }

    fn release_into(&self, slot: Slot) -> Result<(), PortError> {
        self.log.push(format!("release {slot:?}"));
        let held = self.handle.snapshot().held;
        lock(&self.releases).push((slot, held, Instant::now()));
        self.gate.release_into(slot)
    }

    fn expire(&self) -> usize {
        let n = self.gate.expire();
        self.expired.fetch_add(n, Ordering::SeqCst);
        n
    }

    fn open_streams(&self, slot: Slot) -> u64 {
        self.gate.open_streams(slot)
    }

    fn clear_active(&self) {
        self.log.push("clear_active");
        self.gate.clear_active();
    }

    fn retire(&self, slot: Slot) -> Retired {
        let answer = self.gate.retire(slot);
        if answer.is_closed() {
            self.log.push(format!("retired {slot:?}"));
        }
        answer
    }
}

/// The position of the first log line that starts with `step`.
fn at(log: &[String], step: &str) -> usize {
    log.iter()
        .position(|l| l.starts_with(step))
        .unwrap_or_else(|| panic!("`{step}` is not in the log {log:#?}"))
}

#[test]
fn an_all_in_one_swap_feeds_two_hundred_held_streams_to_the_new_child() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("proxy-aio-soak-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("bin")).expect("scratch");
    std::os::unix::fs::symlink(
        std::env::current_exe().expect("this binary"),
        dir.join("bin/kbf"),
    )
    .expect("link kbf");

    let log = Arc::new(Log::default());
    let node = storage_node(&dir, "node");
    let base = free_base();
    let procs = Procs {
        dir: dir.clone(),
        storage: node.field("storage="),
        base,
        log: Arc::clone(&log),
        running: Mutex::new(BTreeMap::new()),
        exits: Mutex::new(BTreeMap::new()),
    };
    let (from, to) = (
        CommitId::new(&"a".repeat(40)).expect("commit"),
        CommitId::new(&"b".repeat(40)).expect("commit"),
    );
    // The old release serves before the swap, in slot A.
    let old = procs.spawn(Slot::A, &from, Start::Active).expect("old");
    let (api_a, api_b) = (operator_api(base, Slot::A), operator_api(base, Slot::B));
    wait_for("the old child's /readyz", || readyz(&api_a));
    assert!(!readyz(&api_b), "nothing runs in slot B yet");

    let public = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("public socket");
    let public_addr: SocketAddr = public.local_addr().expect("addr");
    let (config, pki) = proxy_config(&dir, base);
    let (handle, runner) = Proxy::bind(config, OwnedFd::from(public)).expect("bind");
    let serving = std::thread::spawn(move || runner.run(NoUpdates));

    let spy = Spy {
        gate: ProxyGate::new(handle.clone()),
        handle: handle.clone(),
        log: Arc::clone(&log),
        holds: Mutex::new(Vec::new()),
        releases: Mutex::new(Vec::new()),
        expired: AtomicUsize::new(0),
    };
    // The supervisor's first act: adopt the running child, end the startup hold.
    spy.gate.adopt(Slot::A, None).expect("adopt A");
    let stand = Stand {
        base,
        log: Arc::clone(&log),
        quiesced: AtomicBool::new(false),
        ready_open: AtomicBool::new(false),
        refused_ready: AtomicUsize::new(0),
        old_open: Mutex::new(None),
    };
    let migration = Migration {
        log: Arc::clone(&log),
        reached: AtomicBool::new(false),
        go: AtomicBool::new(false),
    };
    let root = Root {
        to: to.clone(),
        log: Arc::clone(&log),
    };
    let state = dir.join("state");
    std::fs::create_dir_all(&state).expect("state dir");
    let clock = Stoppable {
        inner: SystemClock::new(),
        stop: AtomicBool::new(false),
    };
    let requests = RequestQueue::new();
    let plan = Plan {
        update_id: "u-aio-soak".to_owned(),
        request_id: "r-aio-soak".to_owned(),
        mode: Mode::AllInOne,
        from,
        to,
        nodes: false,
        rollback: false,
        old_slot: Slot::A,
        timing: Timing {
            poll: Duration::from_millis(20),
            bake: Duration::ZERO,
            ready_within: Duration::from_secs(20),
            ..Timing::all_in_one()
        },
    };
    assert_eq!(plan.timing.hold_max, ALL_IN_ONE_HOLD);

    let client_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    // The actions are stored and the client is connected before the swap starts: it stops the
    // old process at once, and `/readyz` is 503 with no slot active.
    let (executes, blobs) = actions("held", HELD);
    let (client, driver) = client_runtime.block_on(async {
        within("the actions", store(&format!("127.0.0.1:{base}"), blobs)).await;
        let (client, driver) = connect(&public_addr.to_string(), &pki.ca_cert).await;
        until_ready(&client).await;
        (client, driver)
    });
    let swap_over = AtomicBool::new(false);
    let (end, notes) = std::thread::scope(|scope| {
        // The deadline: a swap still running at SWAP_BOUND is stopped (its next sleep fails).
        scope.spawn(|| {
            let started = Instant::now();
            while !swap_over.load(Ordering::SeqCst) {
                if started.elapsed() >= SWAP_BOUND {
                    clock.stop.store(true, Ordering::SeqCst);
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });
        let swap = scope.spawn(|| {
            // The recorder is not `Sync`, and only the swap uses it.
            let recorder = FileRecorder::new(&state);
            let ports = Ports {
                child: &procs,
                admin: &stand,
                gate: &spy,
                migrator: &migration,
                recorder: &recorder,
                orders: &root,
                clock: &clock,
                requests: &requests,
            };
            let mut swap = Swap::begin(ports, plan.clone(), Some(old.clone())).expect("begin");
            let end = swap.run();
            swap_over.store(true, Ordering::SeqCst);
            (end, swap.notes().to_vec())
        });
        // A failed check in the body stops the swap before the scope joins it.
        let body = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            client_runtime.block_on(async {

            // The swap has stopped the old process and cleared the active slot, and has not
            // started the new one: it is in the migration.
            wait_for("the swap reaches the migration", || {
                migration.reached.load(Ordering::SeqCst)
            });
            let steps = log.lines();
            assert!(
                at(&steps, "hold") < at(&steps, "quiesce A")
                    && at(&steps, "quiesce A") < at(&steps, "terminate")
                    && at(&steps, "terminate") < at(&steps, "clear_active")
                    && at(&steps, "clear_active") < at(&steps, "retired A")
                    && at(&steps, "retired A") < at(&steps, "migrate"),
                "the stop's order: {steps:#?}"
            );
            assert!(!readyz(&api_a), "the old process is gone");
            assert_eq!(*lock(&stand.old_open), Some(0), "the old child's own count");
            assert_eq!(lock(&procs.exits).values().copied().collect::<Vec<_>>(), [0]);
            let snapshot = handle.snapshot();
            assert_eq!(
                (snapshot.active, snapshot.holding, snapshot.held),
                (None, true, 0)
            );

            // 200 streams arrive now.
            let streams: Vec<_> = executes
                .iter()
                .map(|request| {
                    let (client, request) = (client.clone(), request.clone());
                    tokio::spawn(async move { Open::start(&client, &request).await })
                })
                .collect();
            until("the proxy holds the streams", || handle.snapshot().held >= HELD).await;
            assert_eq!(handle.snapshot().held, HELD, "streams the proxy holds");
            assert!(
                streams.iter().all(|s| !s.is_finished()),
                "a held stream was answered while the old process was gone"
            );
            assert_eq!(handle.snapshot().active, None);

            // The migration ends and the new child starts and is really ready, while the
            // swap's own readiness probe stays shut: the streams are still held.
            migration.go.store(true, Ordering::SeqCst);
            wait_for("the new child's own /readyz", || readyz(&api_b));
            wait_for("the swap's probe of the new child", || {
                stand.refused_ready.load(Ordering::SeqCst) >= 3
            });
            // The streams wait out a stretch of time that a short bound would not survive.
            tokio::time::sleep(DWELL).await;
            let snapshot = handle.snapshot();
            assert_eq!(
                (snapshot.active, snapshot.held, snapshot.per_slot[1].open_streams),
                (None, HELD, 0),
                "after the dwell the streams are not all still held (a release before the swap saw the new \
child ready, or a hold that ran out, shows here)"
            );
            assert_eq!(open_executions(&api_b), 0, "nothing reached the new child yet");
            assert!(streams.iter().all(|s| !s.is_finished()));
            assert!(lock(&spy.releases).is_empty());

            // The swap sees B ready and releases.
            stand.ready_open.store(true, Ordering::SeqCst);
            let mut served = Vec::new();
            let mut refused = Vec::new();
            for stream in streams {
                match within("a held stream", stream).await.expect("task") {
                    Ok(open) => served.push(open),
                    Err(why) => refused.push(why),
                }
            }
            let answers: BTreeSet<_> = refused.iter().collect();
            assert_eq!(
                (served.len(), refused.len(), answers),
                (HELD, 0, BTreeSet::new()),
                "streams served by the new child, streams refused, and how"
            );
            assert_eq!(open_executions(&api_b), HELD as u64, "B's own count");
            let snapshot = handle.snapshot();
            assert_eq!(
                (
                    snapshot.active,
                    snapshot.held,
                    snapshot.stats(GateSlot::A).open_streams,
                    snapshot.stats(GateSlot::B).open_streams,
                ),
                (Some(GateSlot::B), 0, 0, HELD as u64)
            );
            until_ready(&client).await;
            assert!(readyz(&api_b));
            let (code, metrics) = proxy_get(&client, "/metrics").await;
            assert_eq!(code, 200);
            for zero in [
                "kbf_proxy_panics_total",
                "kbf_proxy_hold_rejected_total",
                "kbf_proxy_streams_refused_total",
                "kbf_proxy_unavailable_total",
                "kbf_proxy_upstream_dial_failed_total",
                "kbf_proxy_retries_total",
                "kbf_proxy_hold_expired_total",
                "kbf_proxy_hold_abandoned_total",
                "kbf_proxy_held_streams",
            ] {
                assert!(
                    metrics.contains(&format!("\n{zero} 0\n")),
                    "{zero} is not 0:\n{metrics}"
                );
            }
            assert!(
                metrics.contains("\nkbf_proxy_slot_open_streams{slot=\"a\"} 0\n")
                    && metrics.contains(&format!(
                        "\nkbf_proxy_slot_open_streams{{slot=\"b\"}} {HELD}\n"
                    )),
                "the per-slot gauges:\n{metrics}"
            );

            // They are open and quiet, one connection carried them all, and they end on B.
            let mut served = served;
            for open in &mut served {
                assert!(open.quiet().await, "a served stream ended or spoke");
            }
            assert!(!driver.is_finished(), "the client's one connection stayed up");
            drop(served);
            until("B's executions end", || open_executions(&api_b) == 0).await;
            });
        }));
        if let Err(panic) = body {
            clock.stop.store(true, Ordering::SeqCst);
            swap_over.store(true, Ordering::SeqCst);
            let _ = swap.join();
            eprintln!("the ports' log when the test failed: {:#?}", log.lines());
            std::panic::resume_unwind(panic);
        }
        swap.join().expect("the swap thread")
    });

    let last = log.lines().last().cloned();
    assert_eq!(
        end,
        Ok(Phase::Done),
        "the swap's end (Interrupted: it was stopped at {SWAP_BOUND:?}, stuck after the port call \
         {last:?}), notes {notes:?}"
    );
    assert!(notes.is_empty(), "the swap noted: {notes:?}");
    let steps = log.lines();
    assert!(
        at(&steps, "migrate") < at(&steps, "spawn B")
            && at(&steps, "spawn B") < at(&steps, "ready B")
            && at(&steps, "ready B") < at(&steps, "release B")
            && at(&steps, "release B") < at(&steps, "confirm"),
        "the start's order: {steps:#?}"
    );
    assert_eq!(steps.iter().filter(|l| l.starts_with("release")).count(), 1);
    let holds = lock(&spy.holds).clone();
    let releases = lock(&spy.releases).clone();
    assert_eq!(holds.len(), 1, "one hold");
    assert_eq!(holds[0].0, ALL_IN_ONE_HOLD, "the hold's bound");
    assert_eq!(
        releases.iter().map(|r| (r.0, r.1)).collect::<Vec<_>>(),
        [(Slot::B, HELD)],
        "the release: its slot and the streams it let go"
    );
    let held_for = releases[0].2.saturating_duration_since(holds[0].1);
    assert!(
        held_for < ALL_IN_ONE_HOLD,
        "the hold lasted {held_for:?}, past its bound"
    );
    assert_eq!(
        spy.expired.load(Ordering::SeqCst),
        0,
        "streams the swap expired"
    );
    assert_eq!(handle.snapshot().holds_abandoned, 0);
    handle.shutdown();
    serving.join().expect("the runner thread").expect("run");
    drop(procs);
    let mut node = node;
    assert!(
        node.stop().is_some_and(|status| status.success()),
        "the node exits 0"
    );
}
