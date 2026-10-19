//! The real gate under the whole swap: `kbf-proxy`'s `ActiveSlot` behind [`ProxyGate`], the
//! other ports the fakes of [`super::fakes`]. The fakes run on a virtual clock, so a swap that
//! takes minutes runs at once; the proxy's gate does not care about the clock.

use std::net::TcpListener;
use std::os::fd::OwnedFd;
use std::time::Duration;

use kbf_proxy::config::ProxyConfig;
use kbf_proxy::slot::Bind;
use kbf_proxy::{Proxy, ProxyRunner};

use super::fakes::*;
use super::gate_real::ProxyGate;
use super::model::*;
use super::ports::{Gate, Ports};
use super::swap::Swap;

/// A `[proxy]` table (the keys, without the header) over real files in a scratch directory:
/// a CA, a server certificate, the operator token and a secret path, as `Proxy::bind` reads
/// them. `extra` is more top-level keys.
pub(super) fn proxy_table(name: &str, extra: &str) -> String {
    use rcgen::{
        BasicConstraints, CertificateParams, CertifiedIssuer, ExtendedKeyUsagePurpose, IsCa,
        KeyPair, KeyUsagePurpose, SanType,
    };
    use std::os::unix::fs::PermissionsExt as _;
    let dir = kbf_host::testkit::scratch(&format!("proxy-gate-{name}"));
    let mut ca = CertificateParams::new(Vec::<String>::new()).expect("CA params");
    ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let ca = CertifiedIssuer::self_signed(ca, KeyPair::generate().expect("key")).expect("CA");
    let mut server = CertificateParams::new(Vec::<String>::new()).expect("params");
    server.subject_alt_names = vec![SanType::DnsName("localhost".try_into().expect("ia5"))];
    server.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let key = KeyPair::generate().expect("key");
    let cert = server.signed_by(&key, &ca).expect("server certificate");
    let private = |file: &str, text: &str| {
        let path = dir.join(file);
        std::fs::write(&path, text).expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        path
    };
    let cert = private("server.pem", &cert.pem());
    let key = private("server.key", &key.serialize_pem());
    let ca = private("ca.pem", &ca.pem());
    let token = private("operator.token", "operator-token-0123456789-abcdefghijkl");
    format!(
        "operator_token_file = {token:?}\nserver_cert_file = {cert:?}\n\
         server_key_file = {key:?}\nclient_ca_file = {ca:?}\n\
         proxy_auth_file = {:?}\n{extra}[upstream]\nbase_port = 18000\n",
        dir.join("proxy-auth"),
    )
}

/// A proxy that is bound and not serving, so the gate is real and no socket is needed.
fn proxy() -> (ProxyGate, ProxyRunner) {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let name = format!(
        "real-{}",
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let config =
        ProxyConfig::from_toml_str(&proxy_table(&name, "hold_max_s = 15\n")).expect("config");
    let (handle, runner) = Proxy::bind(config, OwnedFd::from(listener)).expect("proxy");
    (ProxyGate::new(handle), runner)
}

/// A proxy whose children were adopted: slot A serves, the startup hold is over.
fn serving_a() -> (ProxyGate, ProxyRunner) {
    let (gate, runner) = proxy();
    gate.adopt(Slot::A, None).expect("adopt");
    (gate, runner)
}

fn ports<'a>(w: &'a World, gate: &'a ProxyGate) -> Ports<'a> {
    Ports { gate, ..w.ports() }
}

fn run(w: &World, gate: &ProxyGate, mode: Mode, old: ProcIdent) -> Result<Phase, Halt> {
    Swap::begin(ports(w, gate), plan(mode), Some(old))?.run()
}

/// The hold is over, nobody ended it by dropping a guard, and the gate routes to `active`.
fn settled(gate: &ProxyGate, active: Slot, what: &str) {
    let snap = gate.handle.snapshot();
    assert!(!snap.holding, "{what}: the hold is still in force");
    assert_eq!(snap.holds_abandoned, 0, "{what}: a hold ended into nothing");
    assert_eq!(gate.active(), Some(active), "{what}: wrong active slot");
    assert!(
        gate.guard.lock().expect("lock").is_none(),
        "{what}: the gate still keeps a guard after the swap ended"
    );
}

type Setup = fn(&World);

fn never_ready(w: &World) {
    w.script(Script {
        ready_after: None,
        exit_after: None,
    });
}

/// Every way a swap that holds can end, with the slot that must serve afterwards. The
/// hold is the proxy's one atomic word; a swap that reaches its end while still holding, or
/// that lets the guard drop before it set a slot, fails `settled`.
#[test]
fn every_swap_exit_that_holds_ends_in_an_explicit_release_before_the_guard_drops() {
    let frontend: Vec<(&str, Setup, Phase, Slot)> = vec![
        ("success", |_| {}, Phase::Done, Slot::B),
        (
            "takeover fails",
            |w| w.st.borrow_mut().takeover = Takeover::Err,
            Phase::Failed,
            Slot::A,
        ),
        (
            "quiesce too slow",
            |w| w.st.borrow_mut().quiesce_takes = secs(4),
            Phase::Failed,
            Slot::A,
        ),
        (
            "abort while the old child quiesces",
            |w| {
                w.st.borrow_mut().quiesce_takes = Duration::from_millis(2500);
                w.request_at(Duration::from_millis(2150), Request::Abort);
            },
            Phase::Failed,
            Slot::A,
        ),
        (
            "rollback in the bake",
            |w| w.request_at(secs(60), Request::Rollback),
            Phase::RolledBack,
            Slot::A,
        ),
        (
            "rollback that cannot resume the old child",
            |w| {
                w.request_at(secs(60), Request::Rollback);
                w.st.borrow_mut().resume_fails.push(Slot::A);
            },
            Phase::Failed,
            Slot::B,
        ),
        (
            "confirm never lands",
            |w| w.st.borrow_mut().confirm_err = true,
            Phase::Failed,
            Slot::B,
        ),
        ("candidate never ready", never_ready, Phase::Failed, Slot::A),
    ];
    for (what, setup, end, active) in frontend {
        let (w, old) = World::with_old();
        setup(&w);
        let (gate, _runner) = serving_a();
        assert_eq!(run(&w, &gate, Mode::Frontend, old), Ok(end), "{what}");
        settled(&gate, active, what);
    }

    let all_in_one: Vec<(&str, Setup, Phase, Slot)> = vec![
        ("success", |_| {}, Phase::Done, Slot::B),
        (
            "new process never ready",
            never_ready,
            Phase::Failed,
            Slot::A,
        ),
        (
            "migration fails after the stop",
            |w| w.st.borrow_mut().migrate_err = Some("lock timeout".into()),
            Phase::Failed,
            Slot::A,
        ),
        (
            "abort while the old process quiesces",
            |w| {
                w.st.borrow_mut().quiesce_takes = secs(40);
                w.request_at(secs(10), Request::Abort);
            },
            Phase::Failed,
            Slot::A,
        ),
    ];
    for (what, setup, end, active) in all_in_one {
        let (w, old) = World::with_old();
        setup(&w);
        let (gate, _runner) = serving_a();
        assert_eq!(
            run(&w, &gate, Mode::AllInOne, old),
            Ok(end),
            "all-in-one: {what}"
        );
        settled(&gate, active, what);
    }
}

/// A swap that stops on a hold (the proxy halts, here at the record of `quiescing`) leaves
/// the guard in the gate. Dropping the gate then ends the hold into nothing, which is the
/// design's answer to a supervisor that died mid-swap, and it is counted.
#[test]
fn a_swap_that_halts_inside_its_hold_ends_it_into_nothing_when_the_gate_drops() {
    let (w, old) = World::with_old();
    *w.crash_on_record.borrow_mut() = Some(Box::new(|i| i.phase == Phase::Quiescing));
    let (gate, _runner) = serving_a();
    let handle = gate.handle.clone();
    assert!(matches!(
        run(&w, &gate, Mode::Frontend, old),
        Err(Halt::Crashed(_))
    ));
    assert!(
        handle.snapshot().holding,
        "the hold is in force at the halt"
    );
    assert_eq!(handle.snapshot().holds_abandoned, 0);
    drop(gate);
    let snap = handle.snapshot();
    assert!(!snap.holding);
    assert_eq!(snap.holds_abandoned, 1, "ended into nothing, and counted");
    assert_eq!(snap.active, None, "no slot serves after an abandoned hold");
}

/// The hold of the update is the proxy's: a hold already in force (the startup hold of a
/// proxy that adopted nothing yet) is refused, and the swap fails at `hold` before it
/// touches the old child.
#[test]
fn a_hold_the_proxy_refuses_fails_the_update_before_the_old_child_is_touched() {
    let (w, old) = World::with_old();
    let (gate, _runner) = proxy();
    assert_eq!(run(&w, &gate, Mode::Frontend, old), Ok(Phase::Failed));
    assert_eq!(w.last_intent().failure.map(|f| f.step), Some(Step::Hold));
    assert_eq!(w.count("admin:quiesce"), 0);
    assert!(
        gate.handle.snapshot().holding,
        "the startup hold is untouched"
    );
}

/// Adoption after a proxy restart starts held (design section 3): until the children are
/// adopted a stream is held, not bound and not refused; adopting opens the slots, routes to
/// the active one and ends the hold; it runs once.
#[test]
fn adoption_after_a_proxy_restart_starts_held_and_ends_into_the_active_slot() {
    let (gate, _runner) = proxy();
    let slots = gate.handle.slots();
    assert!(
        matches!(slots.bind(), Bind::Held { .. }),
        "held before adoption"
    );
    assert_eq!(gate.active(), None);
    gate.adopt(Slot::B, Some(Slot::A)).expect("adopt");
    assert!(!gate.handle.snapshot().holding);
    assert_eq!(gate.active(), Some(Slot::B));
    assert!(matches!(slots.bind(), Bind::Bound(p) if p.slot() == kbf_proxy::slot::Slot::B));
    assert_eq!(
        slots.generation(kbf_proxy::slot::Slot::A),
        1,
        "the other slot is open"
    );
    let again = gate.adopt(Slot::A, None).expect_err("once only");
    assert!(again.0.contains("already ran"), "{again}");
    assert_eq!(
        gate.active(),
        Some(Slot::B),
        "the second call changed nothing"
    );
}

/// The gate's adapter is a thin map; these pin each answer to the proxy's own.
#[test]
fn retire_and_open_streams_are_the_proxys_answers() {
    let (gate, _runner) = serving_a();
    let slots = gate.handle.slots();
    assert_eq!(gate.retire(Slot::A), Retired::Current);
    assert_eq!(gate.retire(Slot::B), Retired::NeverOpened);
    let permit = match slots.bind() {
        Bind::Bound(p) => p,
        other => panic!("not bound: {other:?}"),
    };
    assert_eq!(gate.open_streams(Slot::A), 1);
    assert_eq!(
        gate.open_streams(Slot::B),
        0,
        "a permit is counted on its own slot"
    );
    gate.clear_active();
    assert_eq!(gate.active(), None);
    assert_eq!(gate.retire(Slot::A), Retired::Busy(1));
    drop(permit);
    assert_eq!(gate.open_streams(Slot::A), 0);
    assert_eq!(gate.retire(Slot::A), Retired::Closed);
    assert_eq!(gate.retire(Slot::A), Retired::AlreadyClosed);
}

/// All-in-one flow 7b on the real gate: the old process stops, no slot is active, its
/// streams drain (here: the permit is dropped by a thread after the stop), and the slot
/// closes before anything starts; the new slot opens at the release; a failed start opens
/// the old slot again under a new generation.
#[test]
fn the_all_in_one_stop_closes_the_old_slot_and_a_failed_start_reopens_it_as_a_new_generation() {
    use kbf_proxy::slot::Slot as G;
    let (w, old) = World::with_old();
    never_ready(&w);
    let (gate, _runner) = serving_a();
    assert_eq!(gate.handle.slots().generation(G::A), 1);
    assert_eq!(
        run(&w, &gate, Mode::AllInOne, old),
        Ok(Phase::Failed),
        "the new process never came up"
    );
    settled(&gate, Slot::A, "fallback");
    assert_eq!(
        gate.handle.slots().generation(G::A),
        2,
        "the old release serves a new generation: the dead process's pool is not reused"
    );
    assert_eq!(gate.handle.slots().generation(G::B), 0, "B never opened");
}
