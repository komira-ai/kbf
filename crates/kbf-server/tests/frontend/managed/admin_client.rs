//! The proxy's `ChildAdmin` (`kbf_installer::proxy::admin::Admin`) against the real admin and
//! operator ports of two managed frontends (`kbf_server::managed`, the code `kbf frontend
//! --admin-listen` runs): a swap's two children side by side, A the old one holding the
//! generation and B a candidate, over one state store. Nothing is a fixture: the client
//! speaks HTTP/1.1 over loopback sockets to the listeners the children bound, and what it
//! reads is what the children's `role` and operator routes answer.
//!
//! The children are tasks of the test process, not operating-system processes, and the
//! store is the in-memory one every test of this directory shares: a SQLite state file is
//! held by one process, and Postgres needs a server. The wire is the same.
//!
//! Some of these tests pin what a child does TODAY where the proxy's design asks for more
//! (`docs/design/frontend-proxy.md`, section 5 and 7). Each says so, and names the change
//! to the child (the "U2c ask") that turns it around; when the child changes, the test that
//! names it changes with it, in the same change.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::Instant;

use kbf_installer::CommitId;
use kbf_installer::proxy::admin::{Admin, AdminError, Deadlines};
use kbf_installer::proxy::child_http::Headers;
use kbf_installer::proxy::model::{
    OpenCounts, Readiness as ChildReadiness, RoleInfo, RoleState, Slot,
};
use kbf_installer::proxy::ports::ChildAdmin;
use kbf_state::Op;

use super::*;

/// The commit B is built from: its release store holds it, A's does not.
const NEW: &str = "fedcba9876543210fedcba9876543210fedcba98";
/// The commit A runs: only A's release store holds it.
const OLD: &str = "0123456789abcdef0123456789abcdef01234567";
/// A commit neither release store holds.
const NOWHERE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
/// Part of the child's `409` for an offer of a commit its release store does not hold
/// complete: the only refusal it has today for a commit that is not its own.
const NO_PLATFORM: &str = "holds no platform of this commit complete";

/// The most any one call of the client may take in these tests: far more than a call on
/// loopback needs, and a bound on the tests that would otherwise hang.
const CALL: Duration = Duration::from_secs(10);

fn deadlines() -> Deadlines {
    Deadlines {
        call: CALL,
        quiesce: CALL,
    }
}

fn commit(text: &str) -> CommitId {
    CommitId::new(text).expect("a commit id")
}

fn holder(state: RoleState) -> RoleInfo {
    RoleInfo {
        holder: true,
        state,
    }
}

fn not_holder(state: RoleState) -> RoleInfo {
    RoleInfo {
        holder: false,
        state,
    }
}

/// The client for the ports of two children: slot A is `a`, slot B is `b`; the operator
/// token is the one the children's `--api-token-file` holds.
fn admin_of(a: &Child, b: &Child, deadlines: Deadlines) -> Arc<Admin> {
    let (a_admin, b_admin, a_api, b_api) = (a.admin, b.admin, a.api, b.api);
    Arc::new(Admin::new(
        move |slot| match slot {
            Slot::A => a_admin,
            Slot::B => b_admin,
        },
        move |slot| match slot {
            Slot::A => a_api,
            Slot::B => b_api,
        },
        Headers::new(),
        || Ok(TOKEN.as_bytes().to_vec()),
        deadlines,
    ))
}

/// A call of the client, on a thread that may block: the client blocks its thread for up to
/// its deadline, and the children are served by this runtime.
async fn run<T: Send + 'static>(
    admin: &Arc<Admin>,
    call: impl FnOnce(&Admin) -> T + Send + 'static,
) -> T {
    let admin = Arc::clone(admin);
    tokio::task::spawn_blocking(move || call(&admin))
        .await
        .expect("a call of the client does not panic")
}

/// A swap's two children on one host: A active, B a candidate; each with an operator token
/// and a release store of its own.
struct Host {
    a: Child,
    b: Child,
    admin: Arc<Admin>,
    store: Arc<Fake>,
    // Kept for the test; dropped last.
    _scratch: Scratch,
    _node: Node,
}

impl Host {
    async fn start(name: &str) -> Self {
        let node = Node::start().await;
        let store = shared();
        let scratch = Scratch::new(name);
        let token = scratch.token_flag();
        let releases = |side: &str| {
            let dir = scratch.0.join(format!("releases-{side}"));
            std::fs::create_dir_all(&dir).expect("a release directory");
            format!("--releases-dir={}", dir.display())
        };
        let (a_releases, b_releases) = (releases("a"), releases("b"));
        let a = Child::start(&managed_flags(&node, &[&token, &a_releases]), &store).await;
        let b = Child::start(
            &managed_flags(&node, &["--await-takeover", &token, &b_releases]),
            &store,
        )
        .await;
        let admin = admin_of(&a, &b, deadlines());
        Self {
            a,
            b,
            admin,
            store,
            _scratch: scratch,
            _node: node,
        }
    }

    async fn run<T: Send + 'static>(&self, call: impl FnOnce(&Admin) -> T + Send + 'static) -> T {
        run(&self.admin, call).await
    }

    /// The `role` of `slot`, as the swap's trait reads it.
    async fn role(&self, slot: Slot) -> RoleInfo {
        self.run(move |admin| admin.role(slot))
            .await
            .expect("a role answer")
    }

    /// Waits until `/readyz` of `slot` says `ready`.
    async fn until_ready(&self, slot: Slot, ready: bool) {
        until("readiness changed", async || {
            self.run(move |admin| admin.try_ready(slot))
                .await
                .is_ok_and(|read| read.readyz == ready)
        })
        .await;
    }

    /// Stores a complete release of `commit` in the release store of the child at `api`:
    /// the tarball of `kbf` for one platform, and its attestation.
    async fn release(&self, api: SocketAddr, commit: &str) {
        let tarball = format!("tarball:kbf:{commit}");
        let bundle = format!("bundle:kbf:{commit}");
        for (name, body) in [("kbf", tarball), ("kbf.attestation", bundle)] {
            let path = format!("/v1/releases/{commit}/linux-x86_64/{name}");
            let request = format!(
                "PUT {path} HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\n\
                 Authorization: Bearer {TOKEN}\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            let (code, answer) = exchange(api, &request).await;
            assert_eq!(code, 201, "{path}: {answer}");
        }
    }
}

/// Catches a client that does not speak the admin wire of the children it drives, across
/// the calls of a swap in the order the swap makes them: `role` of an active child and a
/// candidate; `ready` of each (`/readyz` of the operator port, the one part of readiness
/// the client asks); `open_counts` of an idle child; `quiesce` of the old child;
/// `takeover` of the new, which reads the generation the store is at and sends it as
/// `expect`; the roles after it (the holder moves: B holds, A does not, and A is still
/// quiesced); a `resume` of A, which the child refuses because the store moved on, as
/// `Superseded`; `stand_down` of A; and what a stood-down child reports (`StoodDown`, not
/// ready, still answering the drain read).
///
/// Also pins that the takeover keeps the epoch (daemons that redial keep their leases) and
/// takes exactly one generation, read from the store and not from the client.
///
/// Mutants seen red: `StoodDown` and `Superseded` swapped in `admin.rs`; `holder` derived
/// from the generation alone; the takeover sent without `expect`; the slot ignored by the
/// admin reads (every read from A's port); `ready` or `open_counts` asked on the admin port
/// instead of the operator port; in the child, a takeover that bumps the epoch.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_swap_driven_through_the_admin_calls_moves_the_generation_and_the_holder() {
    let host = Host::start("admin-swap").await;
    let (held, _) = generation(&host.store);
    let epoch_before = epoch(&host.store);

    assert_eq!(host.role(Slot::A).await, holder(RoleState::Active));
    assert_eq!(host.role(Slot::B).await, not_holder(RoleState::Candidate));
    host.until_ready(Slot::B, true).await;
    // `readyz` is the one part of readiness the client asks; the others stay false (fail
    // closed) until the health and capabilities probes exist.
    let ready = host.run(|admin| admin.ready(Slot::A)).await;
    assert_eq!(
        ready,
        Ok(ChildReadiness {
            readyz: true,
            health: false,
            capabilities: false,
        })
    );
    let idle = OpenCounts {
        open_executions: 0,
        queued_operations: 0,
    };
    assert_eq!(host.run(|admin| admin.open_counts(Slot::A)).await, Ok(idle));

    host.run(|admin| admin.quiesce(Slot::A))
        .await
        .expect("quiesce");
    assert_eq!(host.role(Slot::A).await, holder(RoleState::Quiesced));
    host.run(|admin| admin.takeover(Slot::B, CALL))
        .await
        .expect("takeover");
    assert_eq!(
        generation(&host.store).0,
        held + 1,
        "one generation was taken"
    );
    assert_eq!(
        epoch(&host.store),
        epoch_before,
        "a takeover kept the epoch"
    );
    assert_eq!(host.role(Slot::B).await, holder(RoleState::Active));
    assert_eq!(
        host.role(Slot::A).await,
        not_holder(RoleState::Quiesced),
        "the old child still says quiesced, and the store says it no longer holds"
    );

    // A cannot be resumed: the store is at another generation. Typed, and flattened to text
    // for the swap.
    let refused = host.run(|admin| admin.try_resume(Slot::A)).await;
    assert_eq!(refused, Err(AdminError::Superseded { call: "resume" }));
    let flat = host.run(|admin| admin.resume(Slot::A)).await;
    assert!(
        flat.expect_err("a resume is refused")
            .0
            .contains("another writer holds the generation"),
        "the swap sees the reason"
    );

    host.run(|admin| admin.stand_down(Slot::A))
        .await
        .expect("stand_down");
    assert_eq!(host.role(Slot::A).await, not_holder(RoleState::StoodDown));
    host.until_ready(Slot::A, false).await;
    host.until_ready(Slot::B, true).await;
    assert_eq!(host.run(|admin| admin.open_counts(Slot::A)).await, Ok(idle));
    assert_eq!(host.run(|admin| admin.open_counts(Slot::B)).await, Ok(idle));
    assert_eq!(
        host.run(|admin| admin.stand_down(Slot::A)).await,
        Ok(()),
        "a stand-down is idempotent"
    );
}

/// Catches a client that reads a refusal as an answer, or a call to a child that does not
/// serve it as a success: the candidate has the admin port and `/readyz`, and no drain route
/// (the operator API a candidate serves is `/readyz` alone), so `open_counts` of a
/// candidate is `NotFound`, not zeros; a `takeover` of an active child is a `Conflict` that
/// says the call needs a candidate, and moves nothing; and `quiesce`, `resume` and
/// `stand_down` of a candidate are `Conflict`s too, each naming its call.
///
/// Mutant seen red: the takeover sent without `expect` (the refusal is then a `400`, not
/// the child's "needs a candidate"). Not red: `open_counts` asked on the admin port (the
/// admin port has no drain route either, and answers `404` as well).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_call_that_does_not_apply_to_the_child_is_a_typed_refusal() {
    let host = Host::start("admin-refusals").await;

    assert_eq!(
        host.run(|admin| admin.try_open_counts(Slot::B)).await,
        Err(AdminError::NotFound {
            call: "open_counts"
        }),
        "a candidate serves no drain route"
    );
    let refused = host.run(|admin| admin.try_takeover(Slot::A, CALL)).await;
    assert!(
        matches!(&refused, Err(AdminError::Conflict { call: "takeover", error }) if error.contains("candidate")),
        "a takeover of an active child: {refused:?}"
    );
    let flat = host.run(|admin| admin.takeover(Slot::A, CALL)).await;
    assert!(flat.is_err());
    // Nothing moved.
    assert_eq!(host.role(Slot::A).await, holder(RoleState::Active));
    assert_eq!(host.role(Slot::B).await, not_holder(RoleState::Candidate));
    assert_eq!(generation(&host.store).0, 1);

    // A candidate holds no generation and runs nothing: the calls that move a running
    // child do not apply to it.
    for (call, result) in [
        (
            "quiesce",
            host.run(|admin| admin.try_quiesce(Slot::B)).await,
        ),
        ("resume", host.run(|admin| admin.try_resume(Slot::B)).await),
        (
            "stand_down",
            host.run(|admin| admin.try_stand_down(Slot::B)).await,
        ),
    ] {
        assert!(
            matches!(&result, Err(AdminError::Conflict { call: found, .. }) if *found == call),
            "{call} of a candidate: {result:?}"
        );
    }
    assert_eq!(host.role(Slot::B).await, not_holder(RoleState::Candidate));
}

/// Pins TODAY's `resume` against the proxy's rollback (U2c ask: `resume` must work after a
/// stand-down, and must re-take a generation another writer took, or the swap's rollback
/// after step 7 cannot work).
///
/// `ChildAdmin::resume` is documented as "the inverse of quiesce and stand-down". The child
/// resumes from `quiesced` (and `active`, as a no-op) only. So a rollback BEFORE the
/// takeover works (quiesce, then resume, gives the old child back with its generation), and
/// one after a stand-down does not, whether or not anything took the generation: A, stood
/// down with no takeover at all, still holds the generation and `holder` says so, and a
/// `resume` of it is `StoodDown`, not a child that serves again.
///
/// Catches: a `resume` that does not undo a quiesce, one that reports success on a
/// stood-down child (the swap would roll back to a child with no worker listener), and a
/// refusal the client cannot tell from any other 409.
///
/// Mutants seen red: `StoodDown` and `Superseded` swapped in `admin.rs`; `holder` derived
/// from the generation alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_gives_the_old_child_back_before_a_stand_down_and_never_after() {
    let host = Host::start("admin-resume").await;
    let (held, _) = generation(&host.store);

    host.run(|admin| admin.quiesce(Slot::A))
        .await
        .expect("quiesce");
    host.run(|admin| admin.quiesce(Slot::A))
        .await
        .expect("quiesce is idempotent");
    assert_eq!(host.role(Slot::A).await, holder(RoleState::Quiesced));
    host.run(|admin| admin.resume(Slot::A))
        .await
        .expect("resume");
    assert_eq!(host.role(Slot::A).await, holder(RoleState::Active));
    host.run(|admin| admin.resume(Slot::A))
        .await
        .expect("a resume of an active child changes nothing");
    assert_eq!(generation(&host.store).0, held, "the generation moved");

    host.run(|admin| admin.stand_down(Slot::A))
        .await
        .expect("stand_down");
    assert_eq!(
        host.role(Slot::A).await,
        holder(RoleState::StoodDown),
        "nothing took the generation: the stood-down child still holds it"
    );
    let resumed = host.run(|admin| admin.try_resume(Slot::A)).await;
    assert_eq!(resumed, Err(AdminError::StoodDown { call: "resume" }));
    let flat = host.run(|admin| admin.resume(Slot::A)).await;
    assert!(
        flat.expect_err("a resume is refused")
            .0
            .contains("stood down"),
        "the swap sees the reason"
    );
    assert_eq!(host.role(Slot::A).await, holder(RoleState::StoodDown));
    host.until_ready(Slot::A, false).await;
    // Nor does a quiesce apply: the child is out of service for good.
    assert_eq!(
        host.run(|admin| admin.try_quiesce(Slot::A)).await,
        Err(AdminError::StoodDown { call: "quiesce" })
    );

    // The candidate is not hurt by it: it can still take the generation A holds.
    host.run(|admin| admin.takeover(Slot::B, CALL))
        .await
        .expect("takeover");
    assert_eq!(host.role(Slot::B).await, holder(RoleState::Active));
    assert_eq!(host.role(Slot::A).await, not_holder(RoleState::StoodDown));
}

/// Pins TODAY's `takeover` limit (U2c ask: a `limit` in the takeover body, so the child
/// bounds its own start). `ChildAdmin::takeover(slot, limit)` says the limit is the
/// child's; the wire body is `{"expect": N}` and anything else is refused `400`
/// (`deny_unknown_fields`), so the limit can only be the client's: one deadline for the
/// role read and the POST together.
///
/// Catches: a client that sends a field the child refuses (every takeover would be a 400
/// at a real child), a limit that is not enforced (a zero limit that still reaches the
/// child and takes the generation), and a takeover that leaves the candidate changed when
/// it timed out before it sent anything.
///
/// Mutants seen red: the takeover sent without `expect`; the slot ignored by the role read.
/// A mutant that survives, and that nothing a real child shows can tell from the original:
/// the limit not applied to the role read. A zero limit gives the same `Timeout` with
/// nothing taken either way, since the POST gets what is left of the limit, which is
/// nothing; the split of the limit between the two calls is pinned against a slow fixture
/// in `admin_tests.rs`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_takeover_limit_is_the_clients_because_the_wire_has_no_field_for_it() {
    let host = Host::start("admin-limit").await;
    let (held, version) = generation(&host.store);

    let (code, answer) = host
        .b
        .call(
            "POST",
            "/v1/admin/takeover",
            &format!("{{\"expect\": {held}, \"limit_ms\": 5000}}"),
        )
        .await;
    assert_eq!(code, 400, "a child that takes a limit: {answer}");

    // No time at all: the client gives up before it asks, and sends nothing.
    let timed_out = host
        .run(|admin| admin.try_takeover(Slot::B, Duration::ZERO))
        .await;
    assert_eq!(timed_out, Err(AdminError::Timeout { call: "takeover" }));
    assert_eq!(host.role(Slot::B).await, not_holder(RoleState::Candidate));
    assert_eq!(
        generation(&host.store),
        (held, version),
        "a takeover that timed out took the generation"
    );

    // With a limit it takes the generation, and the limit does not shorten the rest.
    let began = Instant::now();
    host.run(|admin| admin.try_takeover(Slot::B, CALL))
        .await
        .expect("takeover");
    assert!(began.elapsed() < CALL, "{:?}", began.elapsed());
    assert_eq!(generation(&host.store).0, held + 1);
}

/// A state store that holds the next write of an operation (an `op/` row, the queueing of
/// an Execute) until it is opened: the Execute is then between its request and its
/// queueing, which is what a quiesce waits for. It can also refuse those writes the way a
/// store does for a writer a newer one has replaced ([`StoreError::Fenced`]) while the
/// generation row is still the writer's own.
struct Holding {
    inner: Arc<Fake>,
    armed: AtomicBool,
    refusing: AtomicBool,
    held: Mutex<Stage>,
    changed: Condvar,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Free,
    Held,
    Open,
}

impl Holding {
    fn new(inner: Arc<Fake>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            armed: AtomicBool::new(false),
            refusing: AtomicBool::new(false),
            held: Mutex::new(Stage::Free),
            changed: Condvar::new(),
        })
    }

    /// Holds the next write of an operation, whenever it comes.
    fn arm(&self) {
        *self.held.lock().unwrap_or_else(PoisonError::into_inner) = Stage::Free;
        self.armed.store(true, Ordering::SeqCst);
    }

    /// Refuses, as `Fenced`, every write that queues an operation from now on.
    fn refuse_submissions(&self) {
        self.refusing.store(true, Ordering::SeqCst);
    }

    fn stage(&self) -> Stage {
        *self.held.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Waits, on the calling thread, until a write is held; panics after ten seconds.
    fn wait_held(&self) {
        let began = Instant::now();
        while self.stage() != Stage::Held {
            assert!(
                began.elapsed() < Duration::from_secs(10),
                "no write of an operation reached the hold"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Lets the held write go.
    fn open(&self) {
        *self.held.lock().unwrap_or_else(PoisonError::into_inner) = Stage::Open;
        self.changed.notify_all();
    }

    /// Holds the calling thread until opened, for at most ten seconds. The store is called
    /// from a worker thread of the runtime, so the runtime is told the thread blocks (it
    /// hands the worker's timers, sockets and queued tasks to another thread): without it
    /// every timer of the test waits as long as the hold does.
    fn hold(&self) {
        tokio::task::block_in_place(|| {
            let mut stage = self.held.lock().unwrap_or_else(PoisonError::into_inner);
            *stage = Stage::Held;
            let (mut stage, _timed_out) = self
                .changed
                .wait_timeout_while(stage, Duration::from_secs(10), |stage| {
                    *stage != Stage::Open
                })
                .unwrap_or_else(PoisonError::into_inner);
            // Over, however it ended: a later look does not find a hold that is gone.
            *stage = Stage::Open;
        });
    }
}

/// Whether `txn` is a submission: the one write that stores an operation's request and
/// takes the next operation number. Other writes of `op/` rows (a scheduler failing an
/// operation nothing can run) are not an Execute between its request and its queueing.
fn queues_an_operation(txn: &Txn) -> bool {
    let puts = |wanted: &dyn Fn(&str) -> bool| {
        txn.ops()
            .iter()
            .any(|op| matches!(op, Op::Put { key, .. } if wanted(key)))
    };
    puts(&|key| key.starts_with("op/")) && puts(&|key| key == "meta/next_op")
}

impl Store for Holding {
    fn apply_batch(
        &self,
        batch: Vec<Txn>,
    ) -> Result<Vec<Result<kbf_state::Commit, Conflict>>, StoreError> {
        if batch.iter().any(queues_an_operation) {
            if self.refusing.load(Ordering::SeqCst) {
                return Err(StoreError::Fenced);
            }
            if self.armed.swap(false, Ordering::SeqCst) {
                self.hold();
            }
        }
        self.inner.apply_batch(batch)
    }
    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        self.inner.get(key)
    }
    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
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

/// A child over `holding`, serving, and the address of its worker listener.
async fn child_over(node: &Node, holding: &Arc<Holding>) -> (Child, SocketAddr) {
    let managed = Managed::start(
        &managed_flags(node, &[]),
        StoreSource::Shared(Arc::clone(holding) as Arc<dyn Store>),
        std::future::pending::<()>(),
    )
    .await
    .expect("starts");
    let view = managed.view();
    let child = Child {
        admin: managed.admin(),
        reapi: view.reapi.expect("a REAPI listener"),
        api: view.api.expect("an operator API"),
        id: view.instance_id,
    };
    tokio::spawn(managed.serve());
    let worker = Child::worker_of(&child.role().await).expect("a worker listener");
    (child, worker)
}

/// Pins TODAY's `quiesce` against the swap's budget.
///
/// The child answers a quiesce when it is quiesced, and it first waits, for at most ten
/// seconds, for the Execute calls between their request and their queueing. The swap gives
/// the call `Deadlines::quiesce` (three seconds). One Execute is held at its queueing write:
/// - **blocking:** a quiesce with a long deadline does not answer while the Execute is held,
///   and answers `Ok` soon after it is let go; an idle child answers at once, and its
///   answer is already `quiesced` (there is no `Quiescing` state: the role never says a
///   quiesce is under way, so a swap cannot poll one);
/// - **a client that gives up:** a quiesce whose deadline is 400 ms returns `Timeout` at
///   its own deadline, and the child DROPS the quiesce with the connection: once the
///   Execute is through the role is still `active`, and a new Execute is queued, not
///   refused. So a `Timeout` from `quiesce` means the child is still serving, and the swap
///   may go on to a `resume` or a second `quiesce` from there. The child drops the call
///   when it next polls the connection: with the held write stalling its runtime that is
///   only once the write is let go, and until then the quiesce is alive. (Whether the
///   connection's closing cancels the call in every state, and not only while it waits, is
///   not shown: a cancel between the fence and the role change would leave a fenced farm
///   that says `active`.)
///
/// U2c ask: answer `202` and report `Quiescing` until the farm is quiet, or bound the wait
/// by what the swap gives it.
///
/// Mutants seen red: the quiesce given the call's deadline instead of its own; in the child,
/// a quiesce that does not wait for a call that is queueing (red in the second half: the
/// farm is then fenced behind the Execute and the quiesce finishes after the client left;
/// the first half alone does not see it, since the fence waits for the same write); a
/// quiesce run in a task of its own, which outlives the connection (second half again); an
/// idle child that waits before it quiesces (the bound on the idle call, first half).
#[tokio::test(flavor = "multi_thread", worker_threads = 16)]
async fn a_quiesce_blocks_while_a_call_is_queueing_and_is_dropped_with_a_client_that_gives_up() {
    let node = Node::start().await;
    let holding = Holding::new(shared());
    let (a, worker) = child_over(&node, &holding).await;
    let client = Client::connect(a.reapi, worker).await;
    let patient = admin_of(&a, &a, deadlines());
    let hasty = admin_of(
        &a,
        &a,
        Deadlines {
            call: CALL,
            quiesce: Duration::from_millis(400),
        },
    );

    // The idle child quiesces at once, and the answer is already `quiesced`. "At once" is
    // inside what the swap gives the call; the child's wait for a call that is queueing is
    // up to ten seconds, which this is not.
    let began = Instant::now();
    run(&patient, |admin| admin.quiesce(Slot::A))
        .await
        .expect("an idle child quiesces");
    assert!(
        began.elapsed() < Deadlines::frontend().quiesce,
        "an idle child waited: {:?}",
        began.elapsed()
    );
    assert_eq!(
        run(&patient, |admin| admin.role(Slot::A)).await,
        Ok(holder(RoleState::Quiesced))
    );
    run(&patient, |admin| admin.resume(Slot::A))
        .await
        .expect("resume");

    // Blocking: an Execute is held at the write that queues it, and a quiesce with time to
    // spare does not answer until it is let go.
    let first = Job::new("an Execute held at its queueing", &[]);
    client.upload(&first.blobs()).await;
    holding.arm();
    let held = tokio::spawn({
        let client = Client::connect(a.reapi, worker).await;
        async move {
            let _ = client.execute_with(&first.action, false).await;
        }
    });
    holding.wait_held();
    // The client of the quiesce and the looks at it are threads of their own: while a write
    // is held, the farm's lock is, and the runtime's tasks that need it wait behind it.
    let quiescing = {
        let patient = Arc::clone(&patient);
        std::thread::spawn(move || patient.try_quiesce(Slot::A))
    };
    let began = Instant::now();
    while began.elapsed() < Duration::from_millis(600) {
        assert!(
            !quiescing.is_finished(),
            "the quiesce answered with a call queueing"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    holding.open();
    let answered = quiescing.join().expect("the client does not panic");
    assert_eq!(answered, Ok(()));
    assert_eq!(
        run(&patient, |admin| admin.role(Slot::A)).await,
        Ok(holder(RoleState::Quiesced))
    );
    tokio::time::timeout(Duration::from_secs(5), held)
        .await
        .expect("the held Execute is through")
        .expect("the task");
    run(&patient, |admin| admin.resume(Slot::A))
        .await
        .expect("resume");

    // A client that gives up: the deadline is the client's, and the child drops the call.
    let second = Job::new("another Execute held at its queueing", &[]);
    client.upload(&second.blobs()).await;
    holding.arm();
    let held = tokio::spawn({
        let client = Client::connect(a.reapi, worker).await;
        async move {
            let _ = client.execute_with(&second.action, false).await;
        }
    });
    holding.wait_held();
    let began = Instant::now();
    let quiesced = {
        let hasty = Arc::clone(&hasty);
        std::thread::spawn(move || hasty.try_quiesce(Slot::A))
            .join()
            .expect("the client does not panic")
    };
    let waited = began.elapsed();
    assert_eq!(quiesced, Err(AdminError::Timeout { call: "quiesce" }));
    assert!(
        waited >= Duration::from_millis(400) && waited < Duration::from_secs(5),
        "the client's deadline, not the child's: {waited:?}"
    );
    // The client is gone (its socket closed when the call returned) and the Execute is still
    // held. There is nothing to wait for before letting it go: while the write is held the
    // child cannot show that it has seen the close (the write holds the farm's lock and a
    // runtime thread, and the quiet look of the quiesce was seen to stop ticking some 100 ms
    // into a hold), and a pause here would only be a guess. What decides the outcome is the
    // order of the child's first poll of the connection after the write is let go: hyper
    // reads the connection (and finds the close) before it polls the handler, whose next
    // quiet look would otherwise finish the quiesce.
    holding.open();
    tokio::time::timeout(Duration::from_secs(5), held)
        .await
        .expect("the held Execute is through")
        .expect("the task");
    // Long enough for the quiet look (10 ms) and the flush, many times over.
    for _ in 0..10 {
        assert_eq!(
            run(&patient, |admin| admin.role(Slot::A)).await,
            Ok(holder(RoleState::Active)),
            "the child went on with a quiesce the client had left"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let third = Job::new("an Execute after a quiesce that timed out", &[]);
    client.upload(&third.blobs()).await;
    assert_eq!(
        execute_refused(&client, &third).await,
        None,
        "the child was fenced by a quiesce that timed out"
    );
}

/// The `N / N / superseded` row of the table in `admin.rs`, against a real child: a child
/// the store refused a write of is no holder, though the generation row still reads as the
/// one it took.
///
/// The child remembers a refusal for good (`superseded` is never cleared, and it takes no
/// more work), and its `role` answer reads the generation row separately, so for as long as
/// the row is not yet the new writer's the two answers disagree. The client trusts the
/// refusal, since the swap must not take a child that can write nothing for the one that
/// holds the generation. The other rows are the swap test's (`N / N / false` holds, `N /
/// another` does not) and the resume test's.
///
/// Here the store refuses the write that queues an Execute with `Fenced`, which is what the
/// store answers a writer a newer one replaced; nothing moves the generation row.
///
/// Catches: a `holder` derived from the generation alone (a superseded child reported as the
/// holder, and a swap that stands its successor down for it). Mutant seen red: `holder`
/// without the `superseded` test in `admin.rs`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_child_the_store_refused_is_no_holder_though_the_generation_row_is_still_its_own() {
    let node = Node::start().await;
    let holding = Holding::new(shared());
    let (a, worker) = child_over(&node, &holding).await;
    let client = Client::connect(a.reapi, worker).await;
    let admin = admin_of(&a, &a, deadlines());

    let view = a.role().await;
    assert_eq!(view["superseded"], false, "{view}");
    assert_eq!(
        run(&admin, |admin| admin.role(Slot::A)).await,
        Ok(holder(RoleState::Active))
    );

    holding.refuse_submissions();
    let job = Job::new("an Execute the store refuses", &[]);
    client.upload(&job.blobs()).await;
    assert_eq!(
        execute_refused(&client, &job).await,
        Some(Code::FailedPrecondition),
        "the child went on with a store that refused its write"
    );
    until("the child says it was superseded", async || {
        a.role().await["superseded"] == true
    })
    .await;

    let view = a.role().await;
    assert_eq!(
        view["generation"], view["held_generation"],
        "the generation row moved: not the row under test: {view}"
    );
    assert_eq!(view["role"], "active", "{view}");
    assert_eq!(
        run(&admin, |admin| admin.role(Slot::A)).await,
        Ok(not_holder(RoleState::Active))
    );
}

/// Pins TODAY's takeover over a holder that is gone (U2c ask: a way to start a candidate
/// that bumps the epoch, or a `takeover` that does when the holder is dead). The proxy's
/// recovery of a host whose both children died spawns the recorded release again and
/// expects it to "take the generation and bump the epoch" (`recover.rs`). The child bumps
/// the epoch only when it is started WITHOUT `--await-takeover` (a cold start); every
/// start the proxy makes has the flag, and `takeover` keeps the epoch, whoever held the
/// generation: so a candidate taking over from a dead holder leaves the epoch as it was,
/// and daemons that redial believe their old leases are still theirs.
///
/// Catches: a client or a child that drifted so that `takeover` bumps the epoch (the
/// swap's takeover would then drop every lease of a live fleet), and keeps the gap on the
/// record next to the cold start that does bump it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_takeover_over_a_dead_holder_keeps_the_epoch_and_only_a_cold_start_bumps_it() {
    let node = Node::start().await;
    let store = shared();
    let source = || StoreSource::Shared(Arc::clone(&store) as Arc<dyn Store>);
    let a = Stoppable::start(&managed_flags(&node, &[]), source()).await;
    let b = Child::start(&managed_flags(&node, &["--await-takeover"]), &store).await;
    let before = epoch(&store);
    let (held, _) = generation(&store);
    // The holder dies: it is stopped, and the store keeps the generation it took.
    assert_eq!(a.stopped().await, Ok(()));

    let admin = admin_of(&b, &b, deadlines());
    run(&admin, |admin| admin.takeover(Slot::A, CALL))
        .await
        .expect("a takeover over a dead holder");
    assert_eq!(generation(&store).0, held + 1);
    assert_eq!(epoch(&store), before, "a takeover bumped the epoch");

    // The way to a bumped epoch is a start with no `--await-takeover`, which no call of
    // `ChildAdmin` makes.
    let c = Child::start(&managed_flags(&node, &[]), &store).await;
    assert_eq!(epoch(&store), before + 1, "a cold start kept the epoch");
    assert_eq!(c.role().await["held_generation"], held + 2);
}

/// Catches an `open_counts` that does not read the child's drain route: the counts of the
/// child's own streams and queue, from the operator port. One Execute with no worker to run
/// it is one open execution and one queued operation; the same counts are what `GET
/// /v1/frontend/drain` says.
///
/// Also pins what the swap's drain waits on after it stood the old child down. The swap
/// retires the old child (SIGTERM, with no timeout behind it) only when `OpenCounts::idle`
/// holds for it. Here A holds an operation that was queued and not yet granted a lease,
/// stands down, and B takes over and runs the operation; A's caller is answered from B's
/// result, `open_executions` falls to 0, and `queued_operations` stays 1 for good: the
/// operation stays in the scheduler's queue of a child that feeds its scheduler nothing,
/// and nothing removes it. So `idle` never holds for a stood-down child that had an
/// operation queued at the takeover, and the swap would wait for it in `draining` until the
/// operator steps in. (`DrainStatus` counts the operations not yet leased to a worker, so
/// an operation that holds a lease is not in the count.)
///
/// U2c ask: a stood-down child counts only what it still has to answer (report 0 queued
/// once stood down), or the drain wait leaves `queued_operations` out for a child that
/// is not the active one.
///
/// Mutants seen red: `open_counts` asked on the admin port; the takeover sent without
/// `expect`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn open_counts_are_the_childs_own_and_a_stood_down_child_keeps_its_queue() {
    let host = Host::start("admin-counts").await;
    let worker = Child::worker_of(&host.a.role().await).expect("a worker listener");
    let client = Client::connect(host.a.reapi, worker).await;
    let job = Job::new("work with no worker to run it", &[]);
    client.upload(&job.blobs()).await;
    let mut ops = client.execute(&job.action).await;

    let busy = OpenCounts {
        open_executions: 1,
        queued_operations: 1,
    };
    until("the child counts its Execute", async || {
        host.run(|admin| admin.open_counts(Slot::A)).await == Ok(busy)
    })
    .await;
    let (code, drain) = get(host.a.api, "/v1/frontend/drain").await;
    assert_eq!(code, 200, "{drain}");
    assert_eq!(drain["open_executions"], 1, "{drain}");
    assert_eq!(drain["queued_operations"], 1, "{drain}");

    // Stood down with the Execute still open (nothing took over to answer it).
    host.run(|admin| admin.stand_down(Slot::A))
        .await
        .expect("stand_down");
    let after = host.run(|admin| admin.open_counts(Slot::A)).await;
    assert_eq!(
        after,
        Ok(busy),
        "a stood-down child still counts the operation it holds"
    );
    assert!(!after.expect("counts").idle());

    // The new holder runs the operation and its caller at A is answered from the result.
    host.run(|admin| admin.takeover(Slot::B, CALL))
        .await
        .expect("takeover");
    let b_worker = Child::worker_of(&host.b.role().await).expect("a worker listener");
    let reached = Client::connect(host.b.reapi, b_worker).await;
    let mut daemon = reached.daemon("node-b", 4, 8).await;
    let lease = daemon.start().await.lease_id.expect("a lease id");
    let result = support::output(&reached, "work with no worker to run it", 0).await;
    assert!(daemon.report(ran(Some(lease), &result)).await.accepted);
    let last = done(&mut ops).await;
    assert_eq!(response(&last).result, Some(result));
    until("the stood-down child has no open Execute", async || {
        host.run(|admin| admin.open_counts(Slot::A))
            .await
            .is_ok_and(|counts| counts.open_executions == 0)
    })
    .await;
    // The caller is answered; the queue is not emptied, however long it is given (the
    // follower looks every 200 ms).
    let stuck = OpenCounts {
        open_executions: 0,
        queued_operations: 1,
    };
    for _ in 0..6 {
        assert_eq!(
            host.run(|admin| admin.open_counts(Slot::A)).await,
            Ok(stuck),
            "a stood-down child answered its caller and counts the operation it queued"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Catches an offer that goes to the wrong child or carries the wrong gates: `post_desired`
/// goes to the operator port of the slot it is given (the new child's, after the bake) with
/// the bearer token and a JSON body, and the child writes the desired release. B holds its
/// own commit; A's release store does not, so an offer that went to A (the old slot) fails.
///
/// Pins what a commit that is not the child's own release gets TODAY: a `409` with an
/// `error` text and no `code`, which the client reads as `Conflict`. Once the child answers
/// `"code": "not_own"` (the desired-commit gate, kbf#1145), the client's `NotOwn` is the
/// answer and this assertion changes to it.
///
/// A wrong token is the child's `401`, a `Status` the swap notes as a failed offer.
///
/// Mutants seen red: the offer sent to the old slot; the bearer token dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn post_desired_reaches_the_new_childs_operator_port_with_its_own_release() {
    let host = Host::start("admin-desired").await;
    host.release(host.a.api, OLD).await;
    host.run(|admin| admin.quiesce(Slot::A))
        .await
        .expect("quiesce");
    host.run(|admin| admin.takeover(Slot::B, CALL))
        .await
        .expect("takeover");
    host.release(host.b.api, NEW).await;

    let new = commit(NEW);
    assert_eq!(
        host.run({
            let new = new.clone();
            move |admin| admin.post_desired(Slot::B, &new)
        })
        .await,
        Ok(())
    );
    let (code, update) = get(host.b.api, "/v1/update").await;
    assert_eq!(code, 200, "{update}");
    assert_eq!(update["desired"]["commit"], NEW, "{update}");
    assert_eq!(update["desired"]["generation"], 1, "{update}");
    assert_eq!(
        host.run({
            let new = new.clone();
            move |admin| admin.post_desired(Slot::B, &new)
        })
        .await,
        Ok(()),
        "an offer is idempotent"
    );

    // The old slot's release store does not hold the new commit: an offer sent there fails.
    let to_old = host
        .run({
            let new = new.clone();
            move |admin| admin.try_post_desired(Slot::A, &new)
        })
        .await;
    assert!(
        matches!(
            &to_old,
            Err(AdminError::Conflict {
                call: "post_desired",
                error
            }) if error.contains(NO_PLATFORM)
        ),
        "{to_old:?}"
    );

    // A commit the child does not hold is a 409 today, with no code.
    let other = commit(NOWHERE);
    let not_own = host
        .run(move |admin| admin.try_post_desired(Slot::B, &other))
        .await;
    assert!(
        matches!(
            &not_own,
            Err(AdminError::Conflict {
                call: "post_desired",
                error
            }) if error.contains(NO_PLATFORM)
        ),
        "{not_own:?}"
    );
    let (_, update) = get(host.b.api, "/v1/update").await;
    assert_eq!(update["desired"]["commit"], NEW, "a refused offer wrote");

    // The same client with another token: the child's gate answers.
    let wrong = Arc::new(Admin::new(
        {
            let b = host.b.admin;
            move |_| b
        },
        {
            let b = host.b.api;
            move |_| b
        },
        Headers::new(),
        || Ok(b"not-the-operator-token-not-the-operator-token".to_vec()),
        deadlines(),
    ));
    let refused = run(&wrong, move |admin| admin.try_post_desired(Slot::B, &new)).await;
    assert!(
        matches!(
            &refused,
            Err(AdminError::Status {
                call: "post_desired",
                code: 401,
                ..
            })
        ),
        "{refused:?}"
    );
}
