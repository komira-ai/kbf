//! The frontend's health against a fake kbf-proxy: the status of `GET /v1/proxy/swap` in
//! every state the rule reads, over real TLS.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use crate::frontend_health::{FrontendHealth, PATH, parse_utc};
use crate::https::{ProxyApi, ProxyTls};
use crate::inventory::{Host, Role};
use crate::ports::{Health, HealthCheck, PortError, PortErrorKind};
use crate::testutil::{FakeProxy, KEY, TOKEN, closed_port, fake_proxy, file, http, pki, scratch};
use crate::version::Version;

/// A package of the series: its commit begins `0123456789ab`.
const TARGET: &str = "0.1.7+g0123456789ab-3";
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const OTHER: &str = "fedcba9876543210fedcba9876543210fedcba98";
const NAME: &str = "proxy.example";

/// The install call, 2026-10-22T08:41:16.500Z, in milliseconds.
const INSTALL_MS: u64 = 1_792_658_476_500;
const BEFORE: &str = "2026-10-22T08:41:15Z";
const AT_INSTALL: &str = "2026-10-22T08:41:16Z";
const AFTER: &str = "2026-10-22T08:41:20Z";

fn host() -> Host {
    Host {
        alias: "fe".to_owned(),
        address: "fe.farm.example".to_owned(),
        role: Role::Frontend,
        host_key: KEY.to_owned(),
    }
}

fn target() -> Version {
    Version::parse(TARGET).expect("version")
}

fn update(state: &str, to: &str, started_at: &str, reason: Option<&str>) -> Value {
    json!({
        "id": "pkg-0123456789ab-1792658480", "seq": 1, "from": OTHER, "to": to,
        "phase": "baking", "state": state, "reason": reason,
        "started_at": started_at, "ended_at": null,
    })
}

fn status(
    boot: &str,
    update: Option<Value>,
    active: Option<&str>,
    installed: Option<&str>,
    pending: Option<&str>,
) -> String {
    json!({
        "proxy_boot": boot, "installed_commit": installed, "active_slot": "b",
        "active_commit": active, "update": update, "pending_binary": pending,
    })
    .to_string()
}

/// A swap that is over and serves the target, begun at `started_at`.
fn done(started_at: &str) -> String {
    status(
        "2026-10-22T08:00:00Z-77",
        Some(update("done", COMMIT, started_at, None)),
        Some(COMMIT),
        Some(COMMIT),
        None,
    )
}

fn adapter(proxy: &FakeProxy, name: &str) -> FrontendHealth {
    let dir = scratch(name);
    let tls = ProxyTls {
        addr: proxy.addr,
        ca_file: file(&dir, "ca.pem", &proxy.ca, 0o644),
        server_name: Some(NAME.to_owned()),
    };
    let token = file(&dir, "token", &format!("{TOKEN}\n"), 0o600);
    let api = ProxyApi::new(&tls, token)
        .expect("api")
        .with_timeout(Duration::from_millis(500));
    FrontendHealth::with_clock(api, Arc::new(|| INSTALL_MS))
}

/// A proxy that answers every call with `body` (status 200), and the adapter for it, whose
/// install call has been announced.
fn serving(name: &str, body: String) -> (FakeProxy, FrontendHealth) {
    let proxy = fake_proxy(NAME, true, move |_, _| Some(http(200, &body)));
    let health = adapter(&proxy, name);
    health.installing(&host());
    (proxy, health)
}

fn check(health: &FrontendHealth) -> Result<Health, PortError> {
    health.check(&host(), &target())
}

fn waiting(health: &Health) -> &str {
    match health {
        Health::Waiting(why) => why,
        other => panic!("expected waiting, got {other:?}"),
    }
}

/// Catches a request the proxy would refuse or a healthy swap that is not recognised: the
/// call is `GET /v1/proxy/swap` with the operator token as a bearer, and a swap that is over,
/// to the target, begun in the second of the install call, served and installed (40 digits
/// against the 12 of the version) with nothing pending is healthy.
#[test]
fn a_finished_swap_to_the_target_is_healthy() {
    let (proxy, health) = serving("fe-ok", done(AT_INSTALL));
    assert_eq!(check(&health), Ok(Health::Healthy));
    let seen = proxy.requests();
    assert_eq!(seen.len(), 1);
    assert!(
        seen[0].starts_with(&format!("GET {PATH} HTTP/1.1\r\n")),
        "{}",
        seen[0]
    );
    assert!(
        seen[0].contains(&format!("Authorization: Bearer {TOKEN}\r\n")),
        "{}",
        seen[0]
    );
    assert!(!seen[0].contains("x-kbf-proxy-auth"));
}

/// Catches a stale `done`: the swap of an earlier install to this very version ended long
/// before this install call, and the proxy still shows it. It is not this install's swap, so
/// the host is not healthy; a swap begun after the call is.
#[test]
fn a_done_swap_from_before_the_install_is_not_healthy() {
    let (_proxy, health) = serving("fe-stale", done(BEFORE));
    let got = check(&health).expect("answer");
    assert!(
        waiting(&got).contains("began before the install"),
        "{got:?}"
    );
    let (_proxy, health) = serving("fe-fresh", done(AFTER));
    assert_eq!(check(&health), Ok(Health::Healthy));
}

/// Catches the stale check being applied when this process made no install call (the updater
/// restarted mid-roll and found the host installed): the rest of the rule still holds, and
/// any `done` swap to the target is accepted; and that an announced call, made after the swap
/// ended, turns the same answer into a wait.
#[test]
fn without_an_install_call_the_freshness_is_not_checked() {
    let proxy = fake_proxy(NAME, true, |_, _| Some(http(200, &done(BEFORE))));
    let health = adapter(&proxy, "fe-no-call");
    assert_eq!(check(&health), Ok(Health::Healthy));
    health.installing(&host());
    assert!(matches!(check(&health), Ok(Health::Waiting(_))));
}

/// Catches a proxy restart being read as a reason to accept, or to refuse: between two reads
/// `proxy_boot` changes (a new process, `seq` back at 1). The first read, mid-swap, waits; the
/// second, of the new process with the swap recorded `done`, is healthy; and a changed boot
/// whose answer does not hold the rule (the old release serves) is not.
#[test]
fn a_changed_proxy_boot_changes_nothing() {
    let bodies = [
        status(
            "2026-10-22T08:30:00Z-10",
            Some(update("running", COMMIT, AFTER, None)),
            Some(OTHER),
            Some(COMMIT),
            None,
        ),
        done(AFTER),
        status(
            "2026-10-22T09:00:00Z-99",
            Some(update("done", COMMIT, AFTER, None)),
            Some(OTHER),
            Some(COMMIT),
            None,
        ),
    ];
    let proxy = fake_proxy(NAME, true, move |n, _| {
        Some(http(200, &bodies[n.min(bodies.len() - 1)]))
    });
    let health = adapter(&proxy, "fe-boot");
    health.installing(&host());
    let first = check(&health).expect("first");
    assert!(waiting(&first).contains("running (baking)"), "{first:?}");
    assert_eq!(check(&health), Ok(Health::Healthy));
    let third = check(&health).expect("third");
    assert!(waiting(&third).contains("serves"), "{third:?}");
}

/// Catches each way a swap that is not over being called healthy: still running; over but a
/// newer kbf waits (`pending_binary`); no update at all, with and without a kbf waiting (a
/// kbf that cannot run shows none); a state this updater does not know. All wait, with the
/// reason in the text.
#[test]
fn what_is_not_over_waits_with_a_reason() {
    let cases = [
        (
            status(
                "b",
                Some(update("running", COMMIT, AFTER, None)),
                Some(OTHER),
                Some(COMMIT),
                None,
            ),
            "the swap is running",
        ),
        (
            status(
                "b",
                Some(update("done", COMMIT, AFTER, None)),
                Some(COMMIT),
                Some(COMMIT),
                Some(OTHER),
            ),
            "a newer kbf waits",
        ),
        (
            status("b", None, Some(OTHER), Some(OTHER), None),
            "no update has started since the install",
        ),
        (
            status("b", None, Some(OTHER), Some(OTHER), Some(COMMIT)),
            "waits for its swap to start",
        ),
        (
            status(
                "b",
                Some(update("paused", COMMIT, AFTER, None)),
                Some(COMMIT),
                Some(COMMIT),
                None,
            ),
            "does not know",
        ),
        (
            status(
                "b",
                Some(update("done", OTHER, AFTER, None)),
                Some(OTHER),
                Some(OTHER),
                None,
            ),
            "not to the target",
        ),
    ];
    for (body, why) in cases {
        let (_proxy, health) = serving("fe-waits", body);
        let got = check(&health).expect("answer");
        assert!(waiting(&got).contains(why), "{why}: {got:?}");
    }
}

/// Catches a swap that is over being healthy while the proxy does not serve the target: the
/// active commit and the installed commit must both be the target's, and the same, whichever
/// is missing or wrong.
#[test]
fn done_is_not_healthy_unless_active_and_installed_are_the_target() {
    for (active, installed) in [
        (None, Some(COMMIT)),
        (Some(COMMIT), None),
        (None, None),
        (Some(OTHER), Some(COMMIT)),
        (Some(COMMIT), Some(OTHER)),
        (Some(OTHER), Some(OTHER)),
        // The same, but not the target's.
        (
            Some("0123456789ac1111111111111111111111111111"),
            Some("0123456789ac1111111111111111111111111111"),
        ),
        // Both name the target but differ after it: not the one recorded commit.
        (
            Some(COMMIT),
            Some("0123456789abffffffffffffffffffffffffffff"),
        ),
    ] {
        let body = status(
            "b",
            Some(update("done", COMMIT, AFTER, None)),
            active,
            installed,
            None,
        );
        let (_proxy, health) = serving("fe-commits", body);
        let got = check(&health).expect("answer");
        assert!(
            waiting(&got).contains("serves"),
            "{active:?} {installed:?}: {got:?}"
        );
    }
}

/// Catches a failure waited out, and one that is not the roll's: an abandoned or refused
/// update to the target since the install fails the roll at once with the proxy's reason (the
/// default words when it gave none). The same words from an update before the install, or to
/// another commit, are not this install's: they wait.
#[test]
fn abandoned_and_refused_fail_with_the_reason_but_only_for_this_install() {
    for (state, word) in [("abandoned", "abandoned"), ("refused", "refused")] {
        let reason = Some("start: the candidate did not become ready");
        let body = status(
            "b",
            Some(update(state, COMMIT, AFTER, reason)),
            Some(OTHER),
            Some(COMMIT),
            None,
        );
        let (_proxy, health) = serving("fe-failed", body);
        assert_eq!(
            check(&health),
            Ok(Health::Abandoned(format!(
                "the swap was {word}: start: the candidate did not become ready"
            )))
        );
        let body = status(
            "b",
            Some(update(state, COMMIT, AFTER, None)),
            None,
            None,
            None,
        );
        let (_proxy, health) = serving("fe-failed-bare", body);
        assert_eq!(
            check(&health),
            Ok(Health::Abandoned(format!(
                "the swap was {word}: no reason given"
            )))
        );
        for (to, started) in [(COMMIT, BEFORE), (OTHER, AFTER)] {
            let body = status(
                "b",
                Some(update(state, to, started, Some("old"))),
                None,
                None,
                None,
            );
            let (_proxy, health) = serving("fe-failed-else", body);
            assert!(
                matches!(check(&health), Ok(Health::Waiting(_))),
                "{state} {to} {started}"
            );
        }
    }
}

/// Catches a proxy without `--kbf-binary` (404) being waited on: it can never answer, so the
/// roll fails at once with a reason that names the flag.
#[test]
fn a_404_fails_the_host_with_a_clear_reason() {
    let proxy = fake_proxy(NAME, true, |_, _| Some(http(404, "{}")));
    let health = adapter(&proxy, "fe-404");
    let Ok(Health::Abandoned(why)) = check(&health) else {
        panic!("expected a failure");
    };
    assert!(why.contains("without --kbf-binary"), "{why}");
}

/// Catches the wrong kind for each way the proxy is not answering or says no: nothing
/// listening, a server that never answers, a 5xx, are `Unreachable` (the roll keeps asking
/// until its bound); the gates' refusals, a surprise status, a body that is not a status and
/// an update without a usable start time are answers (`Other`). No text holds the token or
/// the address.
#[test]
fn unreachable_and_refusals_are_errors_of_the_right_kind_and_leak_nothing() {
    let dead = {
        let tls = ProxyTls {
            addr: closed_port(),
            ca_file: file(&scratch("fe-dead"), "ca.pem", &pki(NAME).0, 0o644),
            server_name: Some(NAME.to_owned()),
        };
        let token = file(&scratch("fe-dead-t"), "token", TOKEN, 0o600);
        FrontendHealth::new(ProxyApi::new(&tls, token).expect("api"))
    };
    let silent = fake_proxy(NAME, true, |_, _| None);
    let cases: Vec<(Result<Health, PortError>, PortErrorKind)> = vec![
        (check(&dead), PortErrorKind::Unreachable),
        (
            check(&adapter(&silent, "fe-silent")),
            PortErrorKind::Unreachable,
        ),
    ];
    for (got, kind) in cases {
        let e = got.expect_err("no answer");
        assert_eq!(e.kind(), kind, "{e}");
        assert!(e.to_string().starts_with("unreachable: kbf-proxy:"), "{e}");
    }
    let bad_start = status(
        "b",
        Some(update("done", COMMIT, "yesterday", None)),
        Some(COMMIT),
        Some(COMMIT),
        None,
    );
    let answers: Vec<(Vec<u8>, PortErrorKind, &str)> = vec![
        (http(503, "{}"), PortErrorKind::Unreachable, "answered 503"),
        (
            http(401, "{}"),
            PortErrorKind::Other,
            "refused the call (401)",
        ),
        (http(403, "{}"), PortErrorKind::Other, "(403)"),
        (http(302, "{}"), PortErrorKind::Other, "answered 302"),
        (
            http(200, "<html>"),
            PortErrorKind::Other,
            "not a swap status",
        ),
        (
            http(200, &bad_start),
            PortErrorKind::Other,
            "no usable started_at",
        ),
    ];
    for (answer, kind, text) in answers {
        let proxy = fake_proxy(NAME, true, move |_, _| Some(answer.clone()));
        let health = adapter(&proxy, "fe-errors");
        let e = check(&health).expect_err(text);
        assert_eq!(
            (e.kind(), e.to_string().contains(text)),
            (kind, true),
            "{text}: {e}"
        );
        for secret in [TOKEN, &proxy.addr.to_string()] {
            assert!(!e.to_string().contains(secret), "{secret} in {e}");
        }
    }
}

/// Catches a version that names no commit being checked against a host: nothing can show the
/// host runs it, so the host fails at once, and the proxy is not even asked.
#[test]
fn a_version_without_a_commit_fails_without_asking() {
    let (proxy, health) = serving("fe-nocommit", done(AFTER));
    let v = Version::parse("2.0-1").expect("version");
    let Ok(Health::Abandoned(why)) = health.check(&host(), &v) else {
        panic!("expected a failure");
    };
    assert!(why.contains("names no commit"), "{why}");
    assert!(proxy.requests().is_empty());
}

/// Catches the time format being read wrongly, the one thing the freshness rule leans on: the
/// epoch, a leap day, the turn of a year and of a century are checked against `date -u`, and
/// every malformed shape is refused (a date that is not a date, a time that is not a time, an
/// offset, fractions, a lower-case z, a sign, extra text, a year before the epoch).
#[test]
fn rfc3339_utc_is_read_to_the_second() {
    for (text, secs) in [
        ("1970-01-01T00:00:00Z", 0),
        ("2000-02-29T23:59:59Z", 951_868_799),
        ("2024-03-01T00:00:00Z", 1_709_251_200),
        ("2026-01-31T12:00:00Z", 1_769_860_800),
        ("2026-10-22T08:41:16Z", 1_792_658_476),
        ("2026-12-31T23:59:59Z", 1_798_761_599),
    ] {
        assert_eq!(parse_utc(text), Some(secs), "{text}");
    }
    for bad in [
        "",
        "yesterday",
        "2026-10-22T08:41:16",
        "2026-10-22T08:41:16+00:00",
        "2026-10-22T08:41:16.5Z",
        "2026-10-22T08:41:16z",
        "2026-10-22 08:41:16Z",
        "+026-10-22T08:41:16Z",
        "2026-10-22T08:41:16Z ",
        "2026-13-01T00:00:00Z",
        "2026-00-01T00:00:00Z",
        "2026-10-32T00:00:00Z",
        "2026-10-00T00:00:00Z",
        "2026-10-22T24:00:00Z",
        "2026-10-22T00:60:00Z",
        "2026-10-22T00:00:60Z",
        "1969-12-31T23:59:59Z",
    ] {
        assert_eq!(parse_utc(bad), None, "{bad:?}");
    }
}
