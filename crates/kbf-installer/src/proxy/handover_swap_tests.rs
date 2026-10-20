//! The handover supervisor: what it does before the proxy is bound, that it is built over the
//! real ports once the slot is filled, and that the root half's own `status.json` is read as
//! "staged" the way the swap needs.

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use kbf_update::record::parse_file_name;

use super::*;
use crate::helper::memory::{Memory, StagedFacts as KeptFacts};
use crate::helper::status;
use crate::proxy::api::model::UpdateId;
use crate::proxy::model::{Intent, Locator, Plan, StagedFacts, Step, Verb};
use crate::proxy::ports::{Recorder, RootOrders};
use crate::proxy::supervisor_swap::rig::Rig;
use crate::state::State;
use crate::testkit::{A, B, c};

const SECRET: &str = "abababababababababababababababababababababababababababababababab";

fn addr(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

fn ports_of(admin: [SocketAddr; 2], operator: [SocketAddr; 2]) -> SlotAddrs {
    SlotAddrs { admin, operator }
}

/// A configuration over a scratch root: the state directory and the spool exist.
fn config(name: &str) -> (HandoverConfig, OrderSpool, PathBuf) {
    let root = kbf_host::testkit::scratch(&format!("handover-swap-{name}"));
    let state_dir = root.join("state");
    fs::create_dir(&state_dir).expect("state dir");
    let cfg = HandoverConfig {
        state_dir: state_dir.clone(),
        orders_dir: root.join("orders"),
        status_file: root.join("status.json"),
        confirm_window_s: 900,
        fetch_timeout_s: 900,
        slots: ports_of([addr(1), addr(1)], [addr(1), addr(1)]),
        operator_token_file: root.join("operator.token"),
        gate: GateSlot::new(),
    };
    let spool = OrderSpool::open(&cfg.orders_dir, &cfg.state_dir).expect("the spool");
    (cfg, spool, root)
}

/// The host as the verb reports it, over the rig's children, with `releases` as `releases/`.
fn host_over(rig: &Arc<Rig>, releases: PathBuf, deaths: &Arc<Mutex<Vec<String>>>) -> Host {
    let by_slot = Arc::clone(rig);
    let sink = Arc::clone(deaths);
    Host {
        gate: rig.clone(),
        child: rig.clone(),
        recorded: Arc::new(move |slot| by_slot.ident(slot)),
        active: Slot::A,
        releases_dir: releases,
        secret: Headers::new(),
        on_death: Arc::new(move |why| sink.lock().unwrap().push(why.to_owned())),
    }
}

fn spec(update_id: &str) -> UpdateSpec {
    UpdateSpec {
        update_id: UpdateId(update_id.to_owned()),
        request_id: format!("req-{update_id}"),
        from: Some(c(A)),
        to: c(B),
        nodes: false,
        rollback: false,
        facts: StagedFacts {
            commit: c(B),
            protocol: 7,
            min_protocol: 5,
            state_schema: 3,
            profile: None,
        },
        order: Locator {
            order_id: 1,
            epoch: 1,
            generation: 1,
            verb: Verb::FrontendUpdate,
            commit: c(B),
            window_s: 900,
        },
    }
}

fn until(what: &str, f: impl Fn() -> bool) {
    let end = Instant::now() + Duration::from_secs(10);
    while !f() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(2));
    }
}

fn built(handover: &Handover) -> bool {
    handover.inner.lock().unwrap().is_some()
}

fn order_ids(dir: &Path) -> Vec<u64> {
    let mut ids: Vec<u64> = fs::read_dir(dir)
        .expect("orders dir")
        .filter_map(|e| parse_file_name(e.expect("entry").file_name().to_str()?))
        .map(|(_, id)| id)
        .collect();
    ids.sort_unstable();
    ids
}

/// Catches a supervisor that acts before the proxy is bound: each call has its own answer
/// (idle status, the words of a refusal, nothing open), nothing is built, and no `update.json`
/// is written.
#[test]
fn before_the_proxy_is_bound_nothing_starts_and_nothing_is_built() {
    let (cfg, spool, _root) = config("unbound");
    let mut handover = Handover::new(&cfg, spool);
    assert_eq!(handover.status(), SupervisorStatus::default());
    assert_eq!(handover.start_blocked().as_deref(), Some(NOT_BOUND));
    assert_eq!(
        handover.start(&spec("u1")),
        Err(SupervisorError::Failed(NOT_BOUND.to_owned()))
    );
    assert_eq!(handover.abort(), Err(SupervisorError::NotOpen));
    assert_eq!(handover.rollback(), Err(SupervisorError::NotOpen));
    assert_eq!(
        handover.force_retire("because"),
        Err(SupervisorError::NotOpen)
    );
    assert!(!built(&handover));
    assert!(!cfg.state_dir.join("update.json").exists());
}

/// Catches a slot that can be set twice, or whose clones do not share what is set: the first
/// host stays, a second fill is refused, and a clone sees the fill of the original.
#[test]
fn the_gate_slot_is_set_once_and_shared_by_its_clones() {
    let slot = GateSlot::new();
    let clone = slot.clone();
    assert!(!slot.is_filled());
    assert_eq!(format!("{slot:?}"), "GateSlot(filled: false)");
    let rig = Rig::new(None);
    let deaths = Arc::new(Mutex::new(Vec::new()));
    let host = |active| Host {
        active,
        ..host_over(&rig, PathBuf::from("/releases"), &deaths)
    };
    assert!(slot.fill(host(Slot::A)));
    assert!(!clone.fill(host(Slot::B)), "a second fill is refused");
    assert!(clone.is_filled());
    assert_eq!(clone.get().map(|h| h.active), Some(Slot::A));
    assert_eq!(format!("{clone:?}"), "GateSlot(filled: true)");
}

/// Catches a supervisor that stays unbound after the fill, or that is built before it: the
/// first call after the fill builds it (here `status`, the call `Core::new` makes) and a call
/// goes to the swap supervisor, which says what it knows (a missing releases directory blocks,
/// an existing one does not).
#[test]
fn the_first_call_after_the_fill_builds_the_swap_supervisor() {
    let (cfg, spool, root) = config("fill");
    let handover = Handover::new(&cfg, spool);
    assert_eq!(handover.start_blocked().as_deref(), Some(NOT_BOUND));
    let rig = Rig::new(None);
    let deaths = Arc::new(Mutex::new(Vec::new()));
    cfg.gate
        .fill(host_over(&rig, root.join("no-releases"), &deaths));
    assert!(!built(&handover), "the fill alone builds nothing");
    assert_eq!(handover.status(), SupervisorStatus::default());
    assert!(built(&handover));
    let why = handover.start_blocked().expect("blocked");
    assert!(why.contains("releases directory"), "{why}");

    let (cfg, spool, root) = config("fill-ok");
    let handover = Handover::new(&cfg, spool);
    cfg.gate.fill(host_over(&rig, root.clone(), &deaths));
    assert_eq!(handover.start_blocked(), None);
}

/// Catches ports of the swap that are not the real ones, or a counter that is not shared: a
/// `start` over the real order spool, recorder and root half. With no `status.json` the swap
/// ends `failed` at step `stage` and sends a `revert` into the same spool, after the order the
/// API wrote (two orders, in order), `update.json` is the state directory's, and the swap
/// thread did not die.
#[test]
fn a_start_runs_over_the_real_spool_recorder_and_root_half() {
    let (cfg, spool, root) = config("start");
    spool
        .revert(&c(A))
        .expect("the API's own order is number 1");
    let mut handover = Handover::new(&cfg, spool);
    let rig = Rig::new(None);
    let deaths = Arc::new(Mutex::new(Vec::new()));
    cfg.gate.fill(host_over(&rig, root.clone(), &deaths));
    assert_eq!(handover.start_blocked(), None);
    handover.start(&spec("u1")).expect("accepted");
    until("the update to end", || {
        handover
            .status()
            .update
            .is_some_and(|u| u.phase == Phase::Failed)
    });
    let update = handover.status().update.expect("shown");
    assert_eq!(update.failure.map(|f| f.step), Some(Step::Stage));
    let ids = order_ids(&cfg.orders_dir);
    assert!(
        ids.len() == 2 && ids[0] < ids[1],
        "the API's order and the swap's revert have two numbers: {ids:?}"
    );
    let on_disk = FileRecorder::load(&cfg.state_dir)
        .expect("update.json reads")
        .expect("update.json is there");
    assert_eq!(
        (on_disk.update_id.as_str(), on_disk.phase),
        ("u1", Phase::Failed)
    );
    until("the swap thread to close", || {
        handover.start_blocked().is_none()
    });
    assert!(deaths.lock().unwrap().is_empty());
}

/// Catches a supervisor built over a state directory other than the proxy's: an update that
/// an earlier process recorded in `update.json` is what the first status shows.
#[test]
fn the_first_status_after_the_fill_reads_the_update_an_earlier_process_recorded() {
    let (cfg, spool, root) = config("resume");
    let plan = Plan {
        update_id: "u0".to_owned(),
        request_id: "req-u0".to_owned(),
        mode: Mode::Frontend,
        from: c(A),
        to: c(B),
        nodes: false,
        rollback: false,
        old_slot: Slot::A,
        timing: Timing::frontend(),
    };
    let mut intent = Intent::begin(&plan, None, 1);
    intent.phase = Phase::Done;
    FileRecorder::new(&cfg.state_dir)
        .record(&intent)
        .expect("recorded");
    let handover = Handover::new(&cfg, spool);
    let rig = Rig::new(None);
    let deaths = Arc::new(Mutex::new(Vec::new()));
    cfg.gate.fill(host_over(&rig, root, &deaths));
    let update = handover
        .status()
        .update
        .expect("the earlier update is shown");
    assert_eq!(
        (update.update_id, update.phase),
        (UpdateId("u0".into()), Phase::Done)
    );
}

// ----- the admin client's wiring -----

/// Catches a secret that does not reach the admin calls: the proxy's header map becomes the
/// client's header lines, name and value unchanged, and an empty map gives no lines.
#[test]
fn the_proxys_admin_headers_become_the_clients() {
    let mut map = hyper::HeaderMap::new();
    map.insert("x-kbf-proxy-auth", SECRET.parse().expect("a header value"));
    assert_eq!(
        admin_secret(&map).lines(),
        format!("x-kbf-proxy-auth: {SECRET}\r\n").as_bytes()
    );
    assert_eq!(admin_secret(&hyper::HeaderMap::new()).lines(), b"");
}

/// A one-request server: answers `answer` to the first request and hands back its head.
fn serve_once(answer: &'static str) -> (SocketAddr, JoinHandle<String>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let at = listener.local_addr().expect("addr");
    let seen = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).expect("read");
            head.push(byte[0]);
        }
        let head = String::from_utf8_lossy(&head).to_string();
        let length = head
            .lines()
            .find_map(|l| {
                l.to_ascii_lowercase()
                    .strip_prefix("content-length: ")
                    .map(str::to_owned)
            })
            .and_then(|n| n.trim().parse::<usize>().ok())
            .unwrap_or(0);
        let mut body = vec![0u8; length];
        stream.read_exact(&mut body).expect("body");
        let reply = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}",
            answer.len()
        );
        stream.write_all(reply.as_bytes()).expect("write");
        head
    });
    (at, seen)
}

/// Catches an admin client pointed at the wrong slot's port, without the proxy's secret, or
/// with the wrong token: a call to slot B's admin port reaches B's (and not A's) with the
/// secret header, and an offer to slot A reaches A's operator port with the token the file
/// holds (the file's newline dropped) and the secret.
#[test]
fn the_admin_client_is_bound_to_each_slots_ports_with_the_secret_and_the_token() {
    let dir = kbf_host::testkit::scratch("handover-swap-admin");
    let token = dir.join("token");
    fs::write(&token, "tok-123\n").expect("token");
    let secret = Headers::new()
        .with("x-kbf-proxy-auth", SECRET.as_bytes())
        .expect("header");

    // Only the two ports that are asked have a server; a call that went anywhere else would
    // find nothing listening and fail.
    let nothing = addr(1);
    let (admin_b, seen_admin_b) =
        serve_once(r#"{"admin_protocol":1,"open_streams":4,"open_sessions":2}"#);
    let (operator_a, seen_operator_a) = serve_once("{}");
    let slots = ports_of([nothing, admin_b], [operator_a, nothing]);
    let admin = admin_of(slots, token, secret);

    let counts = admin.try_open_counts(Slot::B).expect("answered");
    assert_eq!((counts.open_streams, counts.open_sessions), (4, 2));
    let head = seen_admin_b.join().expect("B's admin port was called");
    assert!(head.starts_with("GET /v1/admin/role"), "{head}");
    assert!(
        head.contains(&format!("x-kbf-proxy-auth: {SECRET}")),
        "{head}"
    );

    admin.try_post_desired(Slot::A, &c(A)).expect("answered");
    let head = seen_operator_a
        .join()
        .expect("A's operator port was called");
    assert!(head.starts_with("POST /v1/update/desired"), "{head}");
    assert!(head.contains("authorization: Bearer tok-123\r\n"), "{head}");
    assert!(
        head.contains(&format!("x-kbf-proxy-auth: {SECRET}")),
        "{head}"
    );
}

// ----- how the swap learns that a release is staged -----

/// The root half's `status.json` for a host that runs `A`, as its own writer renders it.
fn root_half_status(state: &State, memory: &Memory) -> (PathBuf, SpoolRootOrders) {
    let dir = kbf_host::testkit::scratch("handover-swap-status");
    let file = dir.join("status.json");
    fs::write(&file, status::render(state, memory)).expect("status.json");
    let spool_dir = dir.join("orders");
    let state_dir = dir.join("state");
    fs::create_dir(&state_dir).expect("state");
    let spool = OrderSpool::open(&spool_dir, &state_dir).expect("spool");
    (file.clone(), SpoolRootOrders::new(spool, file))
}

fn kept(commit: &str) -> KeptFacts {
    KeptFacts {
        commit: c(commit),
        protocol: 7,
        min_protocol: 5,
        state_schema: 3,
    }
}

/// Catches a reader that has drifted from the root half's writer. A release the root half
/// kept (its state names it `staged`, and its facts are in the file) reads as staged with
/// those facts; a release it fetched and verified but did not keep (no `staged` in the
/// state: the pipeline no longer writes it, whoever keeps a release records it) does not,
/// and neither does a state that names it before the facts are published.
#[test]
fn a_release_reads_as_staged_only_once_the_root_half_has_kept_it() {
    let running = State {
        current: Some(c(A)),
        ..State::default()
    };
    let staged_state = State {
        staged: Some(c(B)),
        ..running.clone()
    };
    let with_facts = Memory {
        staged: Some(kept(B)),
        ..Memory::default()
    };

    let (_, orders) = root_half_status(&staged_state, &with_facts);
    let facts = orders.staged().expect("readable").expect("staged");
    assert_eq!(
        (
            facts.commit,
            facts.protocol,
            facts.min_protocol,
            facts.state_schema
        ),
        (c(B), 7, 5, 3)
    );
    assert_eq!(orders.installed().expect("readable"), Some(c(A)));

    // Fetched and verified, not kept: the facts are there, the state does not name it.
    let (_, orders) = root_half_status(&running, &with_facts);
    assert_eq!(orders.staged().expect("readable"), None);

    // Kept, with the facts not yet in the file.
    let (_, orders) = root_half_status(&staged_state, &Memory::default());
    assert_eq!(orders.staged().expect("readable"), None);
}
