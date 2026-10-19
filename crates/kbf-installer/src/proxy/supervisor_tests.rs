//! Adoption at start, on the real gate and real (stand-in) children.

use std::net::TcpListener;
use std::os::fd::OwnedFd;

use kbf_proxy::config::{ProxyConfig, UpstreamTable};
use kbf_proxy::slot::Bind;
use kbf_proxy::{Proxy, ProxyRunner};

use super::*;
use crate::proxy::child_rig::{Cleanup, alive, rig, wait_for};
use crate::proxy::fakes::plan;
use crate::proxy::gate_real_tests::proxy_table;
use crate::proxy::model::{Intent, Mode, Phase};
use crate::proxy::ports::Recorder as _;
use crate::testkit::{A, B, c};

fn gate() -> (ProxyGate, ProxyRunner) {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let name = format!(
        "supervisor-{}",
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let config = ProxyConfig::from_toml_str(&proxy_table(&name, "")).expect("config");
    let (handle, runner) = Proxy::bind(config, OwnedFd::from(listener)).expect("proxy");
    (ProxyGate::new(handle), runner)
}

fn state_dir(rig: &crate::proxy::child_rig::Rig) -> PathBuf {
    rig.dir.parent().expect("under the host").to_owned()
}

fn bound_slot(gate: &ProxyGate) -> Option<kbf_proxy::slot::Slot> {
    match gate.handle.slots().bind() {
        Bind::Bound(permit) => Some(permit.slot()),
        _ => None,
    }
}

/// The restarted proxy finds the child its predecessor recorded, still the recorded process,
/// and routes to it: the startup hold ends into that slot and a stream binds there.
#[test]
fn a_live_recorded_child_is_adopted_and_the_startup_hold_ends_into_its_slot() {
    let rig = rig("supervisor-adopt");
    let ident = rig
        .child
        .spawn(Slot::B, &c(B), Start::Active)
        .expect("spawn");
    let after = rig.restarted();
    let _stop = Cleanup(&rig.child, vec![ident.clone()]);
    let (gate, _runner) = gate();
    assert!(
        matches!(gate.handle.slots().bind(), Bind::Held { .. }),
        "held at first"
    );
    assert_eq!(
        adopt_children(&gate, &after, &state_dir(&rig)),
        Startup::Adopted { active: Slot::B }
    );
    assert_eq!(bound_slot(&gate), Some(kbf_proxy::slot::Slot::B));
    assert!(!gate.handle.snapshot().holding);
}

/// Catches adoption of a child that is gone or no longer the recorded process: the hold is
/// left in force, with the reason.
#[test]
fn a_recorded_child_that_is_gone_is_not_adopted_and_the_hold_stays() {
    let rig = rig("supervisor-gone");
    let ident = rig
        .child
        .spawn(Slot::A, &c(A), Start::Active)
        .expect("spawn");
    let _stop = Cleanup(&rig.child, vec![ident.clone()]);
    rig.child.terminate(&ident).expect("signal");
    wait_for("exit", || !alive(ident.pid));
    let after = rig.restarted();
    let (gate, _runner) = gate();
    let Startup::Held(why) = adopt_children(&gate, &after, &state_dir(&rig)) else {
        panic!("adopted a dead child");
    };
    assert!(why.contains("no frontend child is running"), "{why}");
    assert!(
        gate.handle.snapshot().holding,
        "the startup hold is untouched"
    );
    assert_eq!(gate.active(), None);
}

/// Catches routing to a child of an update nobody is resuming: an update in flight, or a
/// record that cannot be read, leaves the hold even when a child lives.
#[test]
fn an_update_in_flight_or_a_corrupt_record_leaves_the_hold_with_a_live_child() {
    let rig = rig("supervisor-inflight");
    let ident = rig
        .child
        .spawn(Slot::A, &c(A), Start::Active)
        .expect("spawn");
    let _stop = Cleanup(&rig.child, vec![ident.clone()]);
    let after = rig.restarted();
    let state = state_dir(&rig);
    let (gate, _runner) = gate();

    let intent = Intent::begin(&plan(Mode::Frontend), Some(ident.clone()), 1);
    assert_eq!(intent.phase, Phase::Staged);
    FileRecorder::new(&state).record(&intent).expect("record");
    let Startup::Held(why) = adopt_children(&gate, &after, &state) else {
        panic!("adopted under an update");
    };
    assert!(
        why.contains("update is in flight") && why.contains("Staged"),
        "{why}"
    );

    std::fs::write(state.join("update.json"), b"{not json").expect("corrupt");
    let Startup::Held(why) = adopt_children(&gate, &after, &state) else {
        panic!("adopted with a corrupt record");
    };
    assert!(why.contains("corrupt"), "{why}");
    assert!(gate.handle.snapshot().holding);

    // A finished update is history, not an update in flight.
    let mut done = intent;
    done.phase = Phase::Done;
    FileRecorder::new(&state).record(&done).expect("record");
    assert_eq!(
        adopt_children(&gate, &after, &state),
        Startup::Adopted { active: Slot::A }
    );
}

/// Catches two live children being treated as one: with no update in flight and both slots
/// running, nobody knows which holds the generation, so nothing is routed.
#[test]
fn two_live_children_with_no_update_leave_the_hold() {
    let rig = rig("supervisor-two");
    let a = rig.child.spawn(Slot::A, &c(A), Start::Active).expect("a");
    let b = rig.child.spawn(Slot::B, &c(B), Start::Active).expect("b");
    let _stop = Cleanup(&rig.child, vec![a, b]);
    let after = rig.restarted();
    let (gate, _runner) = gate();
    let Startup::Held(why) = adopt_children(&gate, &after, &state_dir(&rig)) else {
        panic!("picked one of two");
    };
    assert!(why.contains("both slots"), "{why}");
    assert_eq!(gate.active(), None);
}

/// The command line of each slot and start, word for word, at base port 18000: the slot's
/// four loopback ports (the proxy's table: REAPI, worker, operator, admin), no `--instance`,
/// and `--await-takeover` on a candidate only. (`tests/proxy_child_argv.rs` parses these
/// same words with the `kbf` binary's own parser.)
#[test]
fn a_frontend_is_started_on_its_slots_ports_and_a_candidate_awaits_the_takeover() {
    let table = UpstreamTable { base_port: 18_000 };
    let argv = frontend_argv(Path::new("/run/kbf-proxy/proxy-auth"), table);
    let words = |slot, start| -> Vec<String> {
        argv(slot, start)
            .into_iter()
            .map(|w| w.to_string_lossy().into_owned())
            .collect()
    };
    let expect = |base: u16, extra: &[&str]| -> Vec<String> {
        let mut want = vec![
            "--mode=frontend".to_owned(),
            format!("--listen=127.0.0.1:{base}"),
            format!("--worker-listen=127.0.0.1:{}", base + 1),
            format!("--api-listen=127.0.0.1:{}", base + 2),
            format!("--admin-listen=127.0.0.1:{}", base + 3),
            "--proxy-auth-file=/run/kbf-proxy/proxy-auth".to_owned(),
        ];
        want.extend(extra.iter().map(|w| (*w).to_owned()));
        want
    };
    assert_eq!(words(Slot::A, Start::Active), expect(18_000, &[]));
    assert_eq!(
        words(Slot::A, Start::Candidate),
        expect(18_000, &["--await-takeover"])
    );
    assert_eq!(words(Slot::B, Start::Active), expect(18_010, &[]));
    assert_eq!(
        words(Slot::B, Start::Candidate),
        expect(18_010, &["--await-takeover"])
    );
    assert_eq!(
        children_dir(Path::new("/var/lib/kbf-proxy")),
        PathBuf::from("/var/lib/kbf-proxy/children")
    );
}

/// Catches an adoption that fails at the gate (the startup hold is gone: ended already, or
/// held by someone else) being reported as adopted.
#[test]
fn an_adoption_the_gate_refuses_is_reported_as_held() {
    let rig = rig("supervisor-refused");
    let ident = rig
        .child
        .spawn(Slot::A, &c(A), Start::Active)
        .expect("spawn");
    let _stop = Cleanup(&rig.child, vec![ident]);
    let after = rig.restarted();
    let (gate, _runner) = gate();
    let _taken = gate.handle.startup_hold().expect("the startup hold");
    let Startup::Held(why) = adopt_children(&gate, &after, &state_dir(&rig)) else {
        panic!("reported adopted without the hold");
    };
    assert!(why.contains("startup hold is gone"), "{why}");
}
