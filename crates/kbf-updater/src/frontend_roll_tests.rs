//! A roll of the frontend with the real [`FrontendHealth`] over a fake kbf-proxy: what the
//! roll does with each answer, to its end (the clock is fake, so the 300 s bound costs the
//! polls and not the wait).

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;

use crate::api::{ErrorCode, Hosts, Roll, RollState};
use crate::frontend_health::FrontendHealth;
use crate::https::{ProxyApi, ProxyTls};
use crate::inventory::Inventory;
use crate::testutil::{KEY, TOKEN, fake_proxy, file, http, scratch};
use crate::updater_tests::{Rig, request};
use crate::{Ports, Timing, Updater};

const VERSION: &str = "0.1.7+g0123456789ab-3";
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const NAME: &str = "proxy.example";
/// 2026-10-22T08:41:16.500Z; the swaps below began at 08:41:20.
const INSTALL_MS: u64 = 1_792_658_476_500;

fn status(update: Option<(&str, Option<&str>)>) -> String {
    json!({
        "proxy_boot": "b", "installed_commit": COMMIT, "active_slot": "b", "active_commit": COMMIT,
        "pending_binary": null,
        "update": update.map(|(state, reason)| json!({
            "id": "pkg-1", "seq": 1, "from": null, "to": COMMIT, "phase": "baking",
            "state": state, "reason": reason, "started_at": "2026-10-22T08:41:20Z",
            "ended_at": null,
        })),
    })
    .to_string()
}

/// Roll the frontend alone to [`VERSION`], the proxy answering call `n` with `answer(n)`.
/// Returns the roll and the number of times the proxy was asked.
fn roll(name: &str, answer: impl Fn(usize) -> Vec<u8> + Send + 'static) -> (Roll, usize, Rig) {
    let proxy = fake_proxy(NAME, true, move |n, _| Some(answer(n)));
    let dir = scratch(name);
    let tls = ProxyTls {
        addr: proxy.addr,
        ca_file: file(&dir, "ca.pem", &proxy.ca, 0o644),
        server_name: Some(NAME.to_owned()),
    };
    let api = ProxyApi::new(&tls, file(&dir, "token", TOKEN, 0o600))
        .expect("api")
        .with_timeout(Duration::from_millis(500));
    let rig = Rig::new(name);
    let ports = Ports {
        safe: Box::new(rig.stop.clone()),
        installer: Box::new(rig.inst.clone()),
        health: Box::new(FrontendHealth::with_clock(api, Arc::new(|| INSTALL_MS))),
        clock: Box::new(rig.clock.clone()),
    };
    let inventory = Inventory::parse(&format!("fe fe.farm.example frontend {KEY}\n")).expect("inv");
    let updater = Updater::open(inventory, rig.store(), ports, Timing::default()).expect("open");
    let req = request(VERSION, Hosts::Aliases(vec!["fe".to_owned()]), "run-1");
    let started = updater.start(&req).expect("start");
    let roll = updater.drive(&started.roll.id).expect("drive");
    let asked = proxy.requests().len();
    (roll, asked, rig)
}

/// Catches a roll that finishes before the swap does, and one that never finishes: the proxy
/// shows nothing, then a swap running, then the swap done; the roll waits through the first
/// two and succeeds at the third, and the install was announced to the adapter first (else
/// the freshness could not be judged).
#[test]
fn the_roll_waits_for_the_swap_and_succeeds_when_it_is_done() {
    let (roll, asked, rig) = roll("fr-swap", |n| match n {
        0 => http(200, &status(None)),
        1 => http(200, &status(Some(("running", None)))),
        _ => http(200, &status(Some(("done", None)))),
    });
    assert_eq!(roll.state, RollState::Done, "{:?}", roll.failure);
    assert_eq!(asked, 3);
    assert_eq!(rig.elapsed(), Duration::from_secs(10));
}

/// Catches a kbf that cannot run (no update is ever shown) holding the roll for ever, or
/// ending it without a reason: after the frontend bound of 300 s the roll fails with
/// `health_timeout` and the reason says no update started since the install.
#[test]
fn no_update_after_the_install_ends_at_the_frontend_bound_with_the_reason() {
    let (roll, asked, rig) = roll("fr-none", |_| http(200, &status(None)));
    assert_eq!(roll.state, RollState::Failed);
    let failure = roll.failure.expect("failure");
    assert_eq!(failure.code, ErrorCode::HealthTimeout);
    assert!(
        failure
            .detail
            .contains("300 s: no update has started since the install"),
        "{}",
        failure.detail
    );
    assert_eq!(rig.elapsed(), Duration::from_secs(300));
    assert_eq!(asked, 61);
}

/// Catches a failed swap being waited out: an abandoned or a refused update fails the roll at
/// the first read with `swap_abandoned` and the proxy's reason; so does a proxy that cannot
/// answer at all (404, no `--kbf-binary`).
#[test]
fn a_failed_swap_and_a_proxy_without_the_status_fail_the_roll_at_once() {
    for (state, reason) in [
        ("abandoned", "start: not ready"),
        ("refused", "fetch: no copy"),
    ] {
        let (roll, asked, rig) = roll("fr-failed", move |_| {
            http(200, &status(Some((state, Some(reason)))))
        });
        let failure = roll.failure.expect("failure");
        assert_eq!(failure.code, ErrorCode::SwapAbandoned);
        assert!(failure.detail.contains(reason), "{}", failure.detail);
        assert_eq!((asked, rig.elapsed()), (1, Duration::ZERO));
    }
    let (roll, asked, _) = roll("fr-404", |_| http(404, "{}"));
    let failure = roll.failure.expect("failure");
    assert_eq!(failure.code, ErrorCode::SwapAbandoned);
    assert!(
        failure.detail.contains("--kbf-binary"),
        "{}",
        failure.detail
    );
    assert_eq!(asked, 1);
}
