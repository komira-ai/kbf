//! What a frontend hands `/readyz` from the state store `bind_frontend` opened.

use std::sync::Arc;

use kbf_front::Owner;
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{ManualClock, Store};
use tonic::transport::Endpoint;

use super::store_of;
use crate::StateStore;
use crate::farm::EpochPolicy;

/// Catches a frontend that probes a different store than the one it opened (or none when
/// one is open): `/readyz` must ask the very store the scheduler writes to, and ask
/// nothing when there is no store.
#[test]
fn readyz_probes_the_store_that_was_opened() {
    let store: Arc<dyn Store> = Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ));
    let state = StateStore {
        store: Arc::clone(&store),
        epoch: EpochPolicy::Keep,
        members: crate::members::MembershipConfig::default(),
        history: None,
    };
    let probed = store_of(Some(&state)).expect("an open store is probed");
    assert!(Arc::ptr_eq(&probed, &store));
    assert!(store_of(None).is_none());
}

/// The flags of a frontend, from `more` on top of a storage node and listeners.
pub(super) fn parsed(more: &[&str]) -> Result<super::FrontendFlags, clap::Error> {
    let mut argv = vec![
        "frontend",
        "--storage=http://127.0.0.1:1",
        "--listen=127.0.0.1:18980",
        "--worker-listen=127.0.0.1:18981",
        "--api-listen=127.0.0.1:18982",
    ];
    argv.extend_from_slice(more);
    super::parse(&super::command().try_get_matches_from(argv)?)
}

/// Catches a frontend that refuses `--placement-load` (scheduling runs in the frontend, so
/// a unit file that sets it would not start), one that ignores it, and one whose default is
/// not `off`.
#[test]
fn the_frontend_takes_placement_load_and_defaults_to_off() {
    use crate::PlacementLoad;
    let mode = |more: &[&str]| parsed(more).expect("flags").server.placement_load;
    assert_eq!(mode(&[]), PlacementLoad::Off);
    assert_eq!(mode(&["--placement-load=off"]), PlacementLoad::Off);
    assert_eq!(mode(&["--placement-load", "on"]), PlacementLoad::On);
    assert!(parsed(&["--placement-load=load"]).is_err());
}

/// Catches a frontend that refuses a whole-node flag (scheduling runs in the frontend, so a
/// unit file that sets one would not start), one that ignores the ceiling, and one that is
/// on without it.
#[test]
fn the_frontend_takes_the_whole_node_flags_and_defaults_to_off() {
    let config = |more: &[&str]| parsed(more).expect("flags").server.whole_node.config();
    assert_eq!(config(&[]), None);
    let on = config(&["--whole-node-max=2", "--whole-node-drain-after-secs=9"]).expect("on");
    assert_eq!((on.max_whole_node, on.drain_after_ms), (2, 9_000));
    assert!(parsed(&["--whole-node-drain-after-secs=9"]).is_err());
}

/// All eleven flags of the removed handover and admin port, each with a value.
const RETIRED: [&str; 11] = [
    "--handover-socket=/run/kbf/control.sock",
    "--instance=b",
    "--handover-quiesce-secs=10",
    "--handover-follow-interval-ms=200",
    "--handover-watchdog-secs=15",
    "--handover-peer-name=kbf.example.internal",
    "--envoy-check-interval-ms=1000",
    "--envoy-check-timeout-ms=1000",
    "--envoy-unhealthy-threshold=2",
    "--admin-listen=127.0.0.1:18983",
    "--await-takeover",
];

/// Asserts that the flags of the removed handover and admin port are read into `retired`
/// and change nothing else: with all eleven given the listeners are where the flags say
/// (`--instance=b` no longer moves them ten ports up) and each flag has a warning; and none
/// of them needs the others (a lone `--instance`, a zero check interval and a peer name
/// without a socket all parse).
#[test]
fn the_retired_handover_flags_are_read_and_change_nothing() {
    let plain = parsed(&[]).expect("flags");
    assert!(plain.retired.warnings().is_empty());
    let old = parsed(&RETIRED).expect("the retired flags parse");
    assert_eq!(old.server.listen.port(), 18980);
    assert_eq!(old.server.worker_listen.port(), 18981);
    assert_eq!(old.server.api_listen, plain.server.api_listen);
    assert_eq!(old.retired.warnings().len(), RETIRED.len());
    assert_eq!(old.retired.instance.as_deref(), Some("b"));
    assert!(old.retired.await_takeover);
    for alone in [
        "--instance=a",
        "--envoy-check-interval-ms=0",
        "--handover-peer-name=x",
    ] {
        parsed(&[alone]).expect(alone);
    }
}

/// Asserts that every retired flag is in the frontend's command, so an old unit's command
/// line is not refused as unknown, and in none of its `--help`, so it does not read as a
/// live setting.
#[test]
fn the_retired_handover_flags_are_in_the_command_and_hidden_from_help() {
    let cmd = super::command();
    let help = cmd.clone().render_long_help().to_string();
    for given in RETIRED {
        let flag = given.split('=').next().expect("a flag");
        assert!(!help.contains(flag), "{flag} in:\n{help}");
        let long = flag.trim_start_matches("--");
        let arg = cmd.get_arguments().find(|a| a.get_long() == Some(long));
        assert!(arg.is_some_and(clap::Arg::is_hide_set), "{flag}");
    }
}

/// Catches: a frontend that starts with a listener it could not bind, for the operator API
/// as for the others (it would serve without the endpoint Envoy and operators ask): the
/// start fails naming the address. Catches: the API socket's bind error dropped.
#[tokio::test]
async fn an_operator_api_address_in_use_fails_the_start() {
    let held = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let api = held.local_addr().expect("address");
    let argv = [
        "frontend".to_owned(),
        "--storage=http://127.0.0.1:1".to_owned(),
        "--listen=127.0.0.1:0".to_owned(),
        "--worker-listen=127.0.0.1:0".to_owned(),
        format!("--api-listen={api}"),
    ];
    let matches = super::command().try_get_matches_from(argv).expect("flags");
    let flags = super::parse(&matches).expect("a frontend's flags");
    let error = super::Prepared::new(&flags).err().expect("in use");
    assert!(
        error.to_string().contains(&format!("bind {api}")),
        "{error}"
    );
}

/// Catches: a frontend that does not bind the three listeners it was asked for, or binds
/// the operator API's on the REAPI's or worker's port. The unit-test build of `Prepared::new`
/// otherwise only ever sees its failure, so the success path (and the coverage of it in this
/// build) is this test's. Catches: the API socket not bound (`api: None`).
#[tokio::test]
async fn a_frontend_binds_its_three_listeners_on_their_own_ports() {
    let argv = [
        "frontend".to_owned(),
        "--storage=http://127.0.0.1:1".to_owned(),
        "--listen=127.0.0.1:0".to_owned(),
        "--worker-listen=127.0.0.1:0".to_owned(),
        "--api-listen=127.0.0.1:0".to_owned(),
    ];
    let matches = super::command().try_get_matches_from(argv).expect("flags");
    let flags = super::parse(&matches).expect("a frontend's flags");
    let prepared = super::Prepared::new(&flags).expect("free ports bind");
    let api = prepared.sockets.api.as_ref().expect("the API is bound");
    let ports = [
        prepared.sockets.reapi.local.port(),
        prepared.sockets.worker.local.port(),
        api.local.port(),
    ];
    assert!(ports.iter().all(|port| *port != 0), "{ports:?}");
    assert!(
        ports[0] != ports[1] && ports[1] != ports[2] && ports[0] != ports[2],
        "{ports:?}"
    );
}

/// Catches: `--auto-promote` on by default (a joiner would then be promoted without an
/// operator on the first release, which the design says must behave as before), its
/// companions turning it on or being refused without it (the config template writes them
/// as live lines, so a refusal would break every frontend started from the template), the
/// flag taken where no membership is kept (it needs the state store), and the values lost
/// between the flags and the setting: the settle time in seconds and the degraded switch.
#[test]
fn auto_promote_is_off_by_default_and_its_companions_need_it() {
    use crate::members::Handoff;
    use std::time::Duration;

    const STORE: &str = "--state-store=sqlite:///x.db";
    assert_eq!(parsed(&[STORE]).expect("parses").handoff, None);
    assert_eq!(parsed(&[]).expect("parses").handoff, None);
    let on = parsed(&[STORE, "--auto-promote"]).expect("parses").handoff;
    assert_eq!(on, Some(Handoff::default()));
    let set = parsed(&[
        STORE,
        "--auto-promote",
        "--handoff-settle-secs=5",
        "--handoff-allow-degraded",
    ])
    .expect("parses")
    .handoff;
    assert_eq!(
        set,
        Some(Handoff {
            settle: Duration::from_secs(5),
            allow_degraded: true,
        })
    );
    let zero = parsed(&[STORE, "--auto-promote", "--handoff-settle-secs=0"]).expect("parses");
    assert_eq!(zero.handoff.map(|h| h.settle), Some(Duration::ZERO));
    // The companions have a live line in the config template (with their defaults), so they
    // must not need the switch: without it they change nothing.
    for alone in [
        vec![STORE, "--handoff-settle-secs=5"],
        vec![STORE, "--handoff-allow-degraded"],
        vec!["--handoff-settle-secs=30"],
    ] {
        assert_eq!(parsed(&alone).expect("parses").handoff, None, "{alone:?}");
    }
    let e = parsed(&["--auto-promote"]).expect_err("no state store");
    assert_eq!(e.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    let help = super::command().render_long_help().to_string();
    for flag in [
        "--auto-promote",
        "--handoff-settle-secs",
        "--handoff-allow-degraded",
    ] {
        assert!(help.contains(flag), "{flag}");
    }
}

/// Asserts that with no drain flag given the waits are the defaults (ten seconds, and no
/// deadline, which `DrainTiming::default` says too), that values are read as
/// seconds, that a deadline has no upper bound, that a wait of zero is allowed and a deadline
/// of zero, a word and a negative number are refused, and that both flags are in the command's help.
#[test]
fn the_drain_flags_have_their_defaults_and_bounds() {
    use crate::sequence::DrainTiming;
    use std::time::Duration;

    let drain = |more: &[&str]| parsed(more).expect("parses").drain;
    assert_eq!(drain(&[]), DrainTiming::default());
    assert_eq!(drain(&[]).deadline, None, "a deadline by default");
    let set = drain(&["--drain-lb-wait=3", "--drain-deadline=45"]);
    let (lb_wait, deadline) = (Duration::from_secs(3), Some(Duration::from_secs(45)));
    assert_eq!(set, DrainTiming { lb_wait, deadline });
    // No upper bound: a year, and a number of seconds only u64 holds.
    let year = drain(&["--drain-deadline=31536000"]).deadline;
    assert_eq!(year, Some(Duration::from_secs(31_536_000)));
    let huge = drain(&["--drain-deadline=18446744073709551615"]).deadline;
    assert_eq!(huge, Some(Duration::from_secs(u64::MAX)));
    assert_eq!(drain(&["--drain-lb-wait=0"]).lb_wait, Duration::ZERO);
    for refused in [
        "--drain-deadline=0",
        "--drain-lb-wait=soon",
        "--drain-deadline=-1",
    ] {
        assert!(parsed(&[refused]).is_err(), "{refused}");
    }
    let help = super::command().render_long_help().to_string();
    for flag in ["--drain-lb-wait", "--drain-deadline"] {
        assert!(help.contains(flag), "{flag}");
    }
}

/// Catches: the setting parsed and dropped before the members that use it (the frontend
/// would take the flag and never promote), or given to members when none is asked for.
#[test]
fn the_members_of_a_frontend_carry_the_setting_it_was_given() {
    use crate::members::Handoff;
    use crate::serve::members_of;

    let state = StateStore {
        store: Arc::new(Fake::with_flaw(
            Arc::new(ManualClock::default()),
            Flaw::None,
        )),
        epoch: EpochPolicy::Keep,
        members: crate::members::MembershipConfig::default(),
        history: None,
    };
    let setting = Handoff {
        settle: std::time::Duration::from_secs(9),
        allow_degraded: true,
    };
    let given = members_of(Some(&state), Some(setting)).expect("members");
    assert_eq!(given.handoff(), Some(setting));
    assert_eq!(
        members_of(Some(&state), None).expect("members").handoff(),
        None
    );
    assert!(members_of(None, Some(setting)).is_none());
}

/// Asserts that `--pack-index` is off by default and is taken beside a Postgres state store,
/// and that a frontend refuses it at parse time, with the line that says why, beside a SQLite
/// state store and with none.
#[test]
fn the_pack_index_flag_needs_a_postgres_state_store() {
    const POSTGRES: &str = "--state-store=postgres:///kbf?host=/run/postgresql";
    let off = parsed(&[POSTGRES]).expect("parses");
    assert!(!off.server.pack_index);
    let on = parsed(&[POSTGRES, "--pack-index"]).expect("parses");
    assert!(on.server.pack_index);
    let sqlite = ["--state-store=sqlite:///x.db", "--pack-index"];
    let e = parsed(&sqlite).expect_err("SQLite");
    assert_eq!(e.kind(), clap::error::ErrorKind::ArgumentConflict);
    let said = e.to_string();
    assert!(said.contains("not SQLite"), "{said}");
    let e = parsed(&["--pack-index"]).expect_err("no state store");
    assert_eq!(e.kind(), clap::error::ErrorKind::ArgumentConflict);
    let said = e.to_string();
    assert!(said.contains("needs --state-store"), "{said}");
}

/// Catches a frontend whose raw ByteStream pass-through does not wait for a storage node
/// that restarts: owners are made holding nothing, so a Read or Write whose owners are all
/// away fails at once, and the proxy must be given the default bound. Catches: `proxy_owners`
/// without `with_hold`. The routed and the single-node owners share that one call after the
/// match, so the farm of one node stands for both.
#[tokio::test]
async fn the_proxys_owners_hold_for_the_default_bound() {
    let bootstrap = Owner::new(
        "storage",
        Endpoint::from_static("http://127.0.0.1:1").connect_lazy(),
    );
    let owners = super::proxy_owners(None, bootstrap);
    assert_eq!(owners.hold().bound(), kbf_route::DEFAULT_HOLD);
}
