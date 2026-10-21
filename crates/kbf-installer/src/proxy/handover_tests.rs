//! The update API's seams on scratch directories: each is the real one, bound to the place it
//! was told, and the supervisor refuses until the proxy is bound.

use std::fs;
use std::path::{Path, PathBuf};

use hyper::StatusCode;
use kbf_update::record::parse_file_name;

use super::*;
use crate::proxy::api::core::{Core, CrashPoint, Fault, Reply};
use crate::proxy::api::model::UpdateRequest;
use crate::proxy::api::traits::Compat;
use crate::proxy::handover_swap::NOT_BOUND;
use crate::proxy::model::Verb;
use crate::proxy::ports::RootOrders;
use crate::proxy::record::Fsync;
use crate::proxy::root_orders::SpoolRootOrders;
use crate::testkit::{A, B, c};

/// A configuration over a scratch directory: the state directory exists, the order spool and
/// the root half's directory do not (the spool is made on open; `status.json` is the root
/// half's).
fn config(name: &str) -> HandoverConfig {
    let root = kbf_host::testkit::scratch(&format!("proxy-handover-{name}"));
    let state_dir = root.join("state");
    fs::create_dir(&state_dir).expect("state dir");
    HandoverConfig {
        state_dir,
        orders_dir: root.join("orders"),
        status_file: root.join("update").join("status.json"),
        confirm_window_s: 900,
        fetch_timeout_s: 900,
        slots: SlotAddrs {
            admin: [addr(18003), addr(18013)],
            operator: [addr(18002), addr(18012)],
        },
        operator_token_file: root.join("operator.token"),
        installer_toml: root.join("installer.toml"),
        gate: GateSlot::new(),
    }
}

fn addr(port: u16) -> std::net::SocketAddr {
    std::net::SocketAddr::from(([127, 0, 0, 1], port))
}

/// The program that is running, installed where `installed` says.
fn binary(installed: &Path) -> RunningBinary {
    let exe = std::env::current_exe().expect("the test binary");
    RunningBinary::from_paths(&exe, installed).expect("identity")
}

/// The root half's `status.json` as an idle host with `A` installed writes it.
fn write_idle_status(cfg: &HandoverConfig) {
    fs::create_dir(cfg.status_file.parent().expect("dir")).expect("update dir");
    fs::write(
        &cfg.status_file,
        format!(r#"{{"installed_commit":"{A}","state":"idle"}}"#),
    )
    .expect("status.json");
}

/// The ids of the records in `dir`, in order.
fn order_ids(dir: &Path) -> Vec<u64> {
    let mut ids: Vec<u64> = fs::read_dir(dir)
        .expect("orders dir")
        .filter_map(|e| parse_file_name(e.expect("entry").file_name().to_str()?))
        .map(|(_, id)| id)
        .collect();
    ids.sort_unstable();
    ids
}

fn request(request_id: &str) -> UpdateRequest {
    UpdateRequest {
        commit: c(B),
        request_id: request_id.to_owned(),
        nodes: false,
        rollback: false,
        source: kbf_update::Source::Fetch,
    }
}

/// Catches seams built with a read of `status.json` at start (a root half that has not
/// written one yet would keep the proxy from serving), or an order spool and counter that are
/// not opened where told: the seams are built with no `status.json`, its read fails when a
/// call makes it, the API answers its status of "no update", and the spool and its counter
/// are there.
#[test]
fn the_seams_start_with_no_status_file_and_the_api_has_no_update() {
    let cfg = config("start");
    let seams = parts(&cfg).expect("the seams");
    assert!(seams.status.read().is_err(), "the status file is not there");
    let mut core = Core::new(seams);
    match core.status() {
        Reply::Status(body) => {
            assert_eq!((body.update_id, body.phase), (None, None));
        }
        other => panic!("not a status: {other:?}"),
    }
    assert!(cfg.orders_dir.is_dir());
    assert!(cfg.state_dir.join("orders.json").is_file());
}

/// Catches a seam bound to the wrong place or the wrong implementation: the status is the
/// file it was told, the request map is the proxy's state directory's (an entry saved there
/// before is found), the clock is the system's, nothing crashes on purpose, and the gate says
/// compatible until R1 is built.
#[test]
fn each_seam_is_bound_to_its_own_place() {
    let cfg = config("bound");
    write_idle_status(&cfg);
    Idempotency::open(&cfg.state_dir)
        .insert("saved-before", &c(B), false, false)
        .expect("a saved request");

    let mut seams = parts(&cfg).expect("the seams");
    let status = seams.status.read().expect("the file it was told");
    assert_eq!(status.installed_commit, Some(c(A)));
    assert!(seams.requests.get("saved-before").is_some());
    assert!(seams.clock.wall_s() > 1_700_000_000, "not the system clock");
    assert_eq!(seams.hook.at(CrashPoint::AfterRequestSaved), Ok(()));
    assert_eq!(seams.compat.check(&c(B), None), Compat::Compatible);
}

/// Catches an `installer.toml` value that is read and then dropped: the confirm window the
/// orders ask for is `confirm_window_s`, and the staging deadline follows `fetch_timeout_s`.
#[test]
fn the_installer_toml_values_reach_the_config() {
    let mut cfg = config("values");
    cfg.confirm_window_s = 300;
    cfg.fetch_timeout_s = 1200;
    let seams = parts(&cfg).expect("the seams");
    assert_eq!(seams.config.window_s, 300);
    assert_eq!(seams.config.staging_deadline.secs(), 1500);
}

/// Catches a staging deadline that is a constant, one that cuts a fetch that is within its own
/// limit, and one past the age an order lives to: the fetch limit and five minutes, at most
/// one second under `MAX_AGE_SECS`.
#[test]
fn the_staging_deadline_is_the_fetch_limit_and_a_margin_up_to_the_orders_age() {
    for (fetch_timeout_s, want) in [
        (60, 360),
        (900, 1200),
        (1498, 1798),
        (1499, 1799),
        (1500, 1799),
        (3600, 1799),
        (u32::MAX, 1799),
    ] {
        assert_eq!(
            staging_deadline(fetch_timeout_s).secs(),
            want,
            "fetch_timeout_s = {fetch_timeout_s}"
        );
    }
    assert_eq!(StagingDeadline::MAX_S, 1799, "the table above is for this");
}

/// Catches a second spool over `orders/`: the API's orders and the orders the swap writes
/// through `SpoolRootOrders` are numbered by one counter. With a clock frozen at 1000 ms the
/// spool the API writes through must hand out 1000, the swap's revert through its clone 1001,
/// and the API's next 1002; a spool of its own would number from the real clock.
#[test]
fn the_api_and_the_swaps_orders_are_numbered_by_one_counter() {
    let cfg = config("one-counter");
    let spool = OrderSpool::open_with(
        &cfg.orders_dir,
        &cfg.state_dir,
        Box::new(|| 1_000),
        Box::new(Fsync),
    )
    .expect("the spool");
    let swap = SpoolRootOrders::new(spool.clone(), &cfg.status_file);
    let mut seams = assemble(&cfg, spool, binary(Path::new("/nonexistent")));

    let first = seams
        .orders
        .write(Verb::FrontendUpdate, &c(B), 900)
        .expect("the API's order");
    swap.revert(&c(B)).expect("the swap's order");
    let second = seams
        .orders
        .write(Verb::Revert, &c(B), 900)
        .expect("the API's next order");
    assert_eq!((first.order_id, second.order_id), (1_000, 1_002));
    assert_eq!(order_ids(&cfg.orders_dir), [1_000, 1_001, 1_002]);
}

/// Catches `restart_pending` answered by anything but the binary captured at start: the
/// program installed at the path it was started from is not pending, and one that is not is.
#[test]
fn restart_pending_is_the_captured_binarys() {
    let cfg = config("restart");
    let exe = std::env::current_exe().expect("the test binary");
    let same = assemble(&cfg, spool_of(&cfg), binary(&exe));
    assert!(!same.restart.restart_pending());
    let cfg = config("restart-other");
    let other = assemble(&cfg, spool_of(&cfg), binary(Path::new("/nonexistent")));
    assert!(other.restart.restart_pending());
}

fn spool_of(cfg: &HandoverConfig) -> OrderSpool {
    OrderSpool::open(&cfg.orders_dir, &cfg.state_dir).expect("the spool")
}

/// Catches a `POST` that reaches the spool while the proxy is not bound: the real seams refuse it
/// with 503 and the reason, `orders/` stays empty and no request id is saved; the status read
/// still answers; and `abort` finds no update.
#[test]
fn a_post_is_refused_and_nothing_reaches_the_spool_or_the_request_map() {
    let cfg = config("post");
    let mut core = Core::new(parts(&cfg).expect("the seams"));
    assert_unbound_refusal(&mut core, "r1");
    assert_eq!(order_ids(&cfg.orders_dir), Vec::<u64>::new());
    assert!(Idempotency::open(&cfg.state_dir).get("r1").is_none());
    assert!(matches!(core.status(), Reply::Status(_)));
    match core.abort() {
        Err(Fault::Api(e)) => assert_eq!(e.code, "no_update_open"),
        other => panic!("{other:?}"),
    }
}

/// Catches a refusal that holds only because the root half has not written a `status.json`
/// (the `POST` of the test above would fail with `status_unreadable` without the unbound
/// supervisor, and write nothing either way): with a status of an idle host and a commit other
/// than the installed one, nothing but the supervisor stands in the way, and the `POST` is
/// still 503 `handover_unavailable`, `orders/` stays empty, no request id is saved, and a
/// second `POST` of the same id is refused the same way (it is not the replay of an accepted
/// update).
#[test]
fn a_post_is_refused_by_the_unbound_supervisor_when_the_root_half_is_idle() {
    let cfg = config("post-idle");
    write_idle_status(&cfg);
    let mut core = Core::new(parts(&cfg).expect("the seams"));
    assert_unbound_refusal(&mut core, "r1");
    assert_unbound_refusal(&mut core, "r1");
    assert_eq!(order_ids(&cfg.orders_dir), Vec::<u64>::new());
    assert!(Idempotency::open(&cfg.state_dir).get("r1").is_none());
}

/// The `POST` of `request_id` is refused as an API whose proxy is not bound refuses it.
fn assert_unbound_refusal(core: &mut Core, request_id: &str) {
    let refused = match core.post(request(request_id)) {
        Err(Fault::Api(e)) => e,
        other => panic!("not refused: {other:?}"),
    };
    assert_eq!(
        (refused.status, refused.code, refused.message.as_str()),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "handover_unavailable",
            NOT_BOUND
        )
    );
}

/// Catches an order spool that cannot be opened being passed over: a file where `orders/` is
/// and a state directory that is not there each fail `parts` with the spool's reason, and the
/// first names the directory.
#[test]
fn a_spool_that_cannot_be_opened_is_an_error_that_names_it() {
    let cfg = config("spool-fails");
    fs::write(&cfg.orders_dir, b"a file").expect("a file in the way");
    match parts(&cfg).err() {
        Some(e @ HandoverError::Orders { .. }) => {
            let text = e.to_string();
            assert!(
                text.contains(&cfg.orders_dir.display().to_string()),
                "{text}"
            );
        }
        other => panic!("{other:?}"),
    }

    let cfg = HandoverConfig {
        state_dir: PathBuf::from("/nonexistent/state"),
        ..config("spool-fails-state")
    };
    match parts(&cfg).err() {
        Some(HandoverError::Orders {
            source: SpoolError::Counter(_),
            ..
        }) => {}
        other => panic!("{other:?}"),
    }
}
