//! Flags, start-up failures, and the binary's own wiring (the real safe-to-stop, the installer
//! and health check that are not built) over a real socket.

use std::os::unix::net::UnixListener;

use serde_json::{Value, json};

use crate::api::{ErrorCode, Roll, RollState};
use crate::daemon::{
    Config, build_ports, drive_loop, parse_flags, parse_operator_url, run_config, start,
    stderr_sink,
};
use crate::fakes::{Event, FakeClock, FakeHealth, FakeInstaller, FakeSafeToStop, Log};
use crate::inventory::{Host, Role};
use crate::not_built::{NOT_BUILT, NotBuiltHealth, NotBuiltInstaller};
use crate::ports::{HealthCheck, Installer, PortErrorKind};
use crate::record::RollStore;
use crate::server::Recent;
use crate::testutil::{
    FakeApi, KEY, Lines, TOKEN, call, eventually, fake_api, file, http, own_uid, scratch,
};
use crate::version::Version;
use crate::{Ports, Timing, Updater, run_with};

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_owned()).collect()
}

const FLAGS: [&str; 12] = [
    "--state-dir",
    "/s",
    "--inventory",
    "/i",
    "--socket",
    "/sock",
    "--proxy-uid",
    "993",
    "--operator-api",
    "http://127.0.0.1:8080",
    "--token-file",
    "/t",
];

/// Catches a flag that is not read, read from the wrong place, or accepted twice, and each of
/// the six being optional: every flag is required, `--flag=value` works, an unknown argument
/// and a flag with no value are errors, and the inventory owner is root.
#[test]
fn the_six_flags_are_required_and_exact() {
    let cfg = parse_flags(&args(&FLAGS)).expect("flags");
    assert_eq!(
        (
            cfg.state_dir.to_str(),
            cfg.inventory.to_str(),
            cfg.socket.to_str(),
            cfg.token_file.to_str()
        ),
        (Some("/s"), Some("/i"), Some("/sock"), Some("/t"))
    );
    assert_eq!(
        (cfg.proxy_uid, cfg.inventory_owner, cfg.operator_api.port()),
        (993, 0, 8080)
    );
    let eq: Vec<String> = FLAGS
        .chunks(2)
        .map(|p| format!("{}={}", p[0], p[1]))
        .collect();
    assert_eq!(parse_flags(&eq).expect("= form").socket, cfg.socket);
    for (i, name) in FLAGS.iter().step_by(2).enumerate() {
        let mut without = FLAGS.to_vec();
        without.drain(i * 2..i * 2 + 2);
        let e = parse_flags(&args(&without)).expect_err(name);
        assert_eq!(e, format!("{name} is required"));
    }
    let twice = [&FLAGS[..], &["--socket", "/other"]].concat();
    assert_eq!(
        parse_flags(&args(&twice)).expect_err("twice"),
        "--socket is given twice"
    );
    assert_eq!(
        parse_flags(&args(&["serve"])).expect_err("unknown"),
        "unknown argument serve"
    );
    assert_eq!(
        parse_flags(&args(&["--state-dir"])).expect_err("no value"),
        "--state-dir needs a value"
    );
    let mut bad_uid = FLAGS.to_vec();
    bad_uid[7] = "root";
    assert_eq!(
        parse_flags(&args(&bad_uid)).expect_err("uid"),
        "--proxy-uid must be a number"
    );
    bad_uid[7] = "-1";
    assert!(parse_flags(&args(&bad_uid)).is_err());
}

/// Catches the operator token being sent anywhere but loopback in clear: a non-loopback IP, a
/// name (no resolver is used), `https`, no scheme and a path or junk are refused; both
/// loopback families are taken.
#[test]
fn the_operator_api_must_be_a_loopback_address() {
    for ok in [
        "http://127.0.0.1:8080",
        "http://127.0.0.1:8080/",
        "http://[::1]:9",
    ] {
        assert!(parse_operator_url(ok).is_ok(), "{ok}");
    }
    assert_eq!(
        parse_operator_url("http://[::ffff:127.0.0.1]:9").map(|a| a.port()),
        Ok(9),
        "a mapped loopback is loopback"
    );
    for (bad, why) in [
        ("http://192.0.2.5:8080", "loopback"),
        ("http://0.0.0.0:8080", "loopback"),
        ("http://[2001:db8::1]:80", "loopback"),
        ("http://localhost:8080", "IP address"),
        ("http://example.com:80", "IP address"),
        ("http://127.0.0.1", "IP address"),
        ("http://127.0.0.1:8080/v1", "IP address"),
        ("https://127.0.0.1:8080", "http://"),
        ("127.0.0.1:8080", "http://"),
    ] {
        let e = parse_operator_url(bad).expect_err(bad);
        assert!(e.contains(why), "{bad}: {e}");
    }
}

/// A config over a scratch directory with `inventory` and a good token file.
fn config(name: &str, inventory: &str, api: &FakeApi) -> Config {
    let dir = scratch(name);
    let state = dir.join("state");
    std::fs::create_dir(&state).expect("state");
    Config {
        state_dir: state,
        inventory: file(&dir, "inventory", &inventory.replace("KEY", KEY), 0o644),
        inventory_owner: own_uid(),
        socket: dir.join("api.sock"),
        proxy_uid: own_uid(),
        operator_api: api.addr,
        token_file: file(&dir, "token", TOKEN, 0o600),
    }
}

fn wait_failed(socket: &std::path::Path) -> Roll {
    let mut last = None;
    eventually(|| {
        let roll: Roll =
            serde_json::from_value(call(socket, "GET", "/v1/updater/rolls/roll-1", "").1)
                .expect("roll");
        let over = roll.state != RollState::Running;
        last = Some(roll);
        over
    });
    last.expect("roll")
}

fn post(socket: &std::path::Path) -> (u16, Value) {
    let body = json!({"package": "kbf", "version": "2.0-1", "idempotency_key": "run-1"});
    call(socket, "POST", "/v1/updater/rolls", &body.to_string())
}

/// Catches the not-built installer letting a roll touch a host: with the binary's own wiring,
/// a POST is accepted (202) and the roll FAILS at the frontend's install with the reason in
/// the failure, no later host is asked about (the fake operator API sees no request at all),
/// and the frontend, which has no safe-to-stop, made none either.
#[test]
fn the_binary_wiring_accepts_a_roll_and_fails_safe_at_the_first_install() {
    let api = fake_api(vec![Some(http(
        200,
        "{\"safe\":true,\"reasons\":[],\"planned_down\":false}",
    ))]);
    let cfg = config(
        "unbuilt",
        "fe fe.farm.example frontend KEY\nst1 st1.farm.example storage KEY\n",
        &api,
    );
    let daemon = start(&cfg).expect("start");
    let (status, body) = post(&cfg.socket);
    assert_eq!((status, &body["state"]), (202, &json!("running")), "{body}");
    let roll = wait_failed(&cfg.socket);
    assert_eq!(roll.state, RollState::Failed);
    let failure = roll.failure.expect("failure");
    assert_eq!(
        (failure.alias.as_str(), failure.code),
        ("fe", ErrorCode::InstallFailed)
    );
    assert!(
        failure.detail.contains("no host installer yet"),
        "{}",
        failure.detail
    );
    assert!(failure.detail.contains("no host was touched"));
    let phases: Vec<_> = roll
        .hosts
        .iter()
        .map(|h| format!("{:?}", h.phase))
        .collect();
    assert_eq!(phases, ["Failed", "Pending"]);
    assert!(
        api.requests().is_empty(),
        "no host was asked about: {:?}",
        api.requests()
    );
    daemon.stopper().stop();
    daemon.wait();
}

/// Catches the real safe-to-stop not being wired into the roll: a node-only roll asks the
/// operator API once, with the token from the file, and is then stopped by the not-built
/// installer, still before any host is touched.
#[test]
fn a_node_is_checked_with_the_token_and_then_refused_by_the_installer() {
    let api = fake_api(vec![Some(http(
        200,
        "{\"safe\":true,\"reasons\":[],\"planned_down\":false}",
    ))]);
    let cfg = config("unbuilt2", "st1 st1.farm.example storage KEY\n", &api);
    let daemon = start(&cfg).expect("start");
    assert_eq!(post(&cfg.socket).0, 202);
    let roll = wait_failed(&cfg.socket);
    let failure = roll.failure.expect("failure");
    assert_eq!(
        (failure.alias.as_str(), failure.code),
        ("st1", ErrorCode::InstallFailed)
    );
    let seen = api.requests();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert!(seen[0].starts_with("GET /v1/members/st1/safe-to-stop "));
    assert!(seen[0].contains(&format!("Bearer {TOKEN}")));
    // The token is in no audit line and no answer.
    assert!(
        !call(&cfg.socket, "GET", "/v1/updater/rolls", "")
            .1
            .to_string()
            .contains(TOKEN)
    );
    daemon.stopper().stop();
    daemon.wait();
}

/// Catches the refusing ports answering with anything but a refusal.
#[test]
fn the_not_built_ports_refuse_everything() {
    let host = Host {
        alias: "st1".into(),
        address: "st1.farm.example".into(),
        role: Role::Storage,
        host_key: KEY.into(),
    };
    let v = Version::parse("2.0").expect("version");
    let e = NotBuiltInstaller.install(&host, &v).expect_err("install");
    assert_eq!(
        (e.kind(), e.to_string().as_str()),
        (PortErrorKind::Other, NOT_BUILT)
    );
    assert_eq!(
        NotBuiltInstaller
            .query(&host)
            .expect_err("query")
            .to_string(),
        NOT_BUILT
    );
    let e = NotBuiltHealth.check(&host, &v).expect_err("health");
    assert!(e.to_string().contains("no health check yet"));
}

/// Catches start-up that goes on over something wrong: an inventory that is not owned by the
/// expected user or is missing, a state directory that does not exist or is locked by another
/// updater, a corrupt record, a socket someone listens at, and a token file that is no good.
/// Each is an error naming what failed, and the socket is not left bound.
#[test]
fn start_up_refuses_what_it_cannot_trust() {
    let api = fake_api(vec![Some(http(200, "{}"))]);
    let good = "st1 st1.farm.example storage KEY\n";

    let mut cfg = config("bad1", good, &api);
    cfg.inventory_owner = own_uid() + 1;
    let e = start(&cfg).err().expect("owner");
    assert!(e.starts_with("inventory "), "{e}");
    cfg.inventory = cfg.inventory.with_file_name("missing");
    assert!(
        start(&cfg)
            .err()
            .expect("missing")
            .starts_with("inventory ")
    );

    let cfg = config("bad2", good, &api);
    let held = RollStore::open(&cfg.state_dir).expect("lock");
    let e = start(&cfg).err().expect("locked");
    assert!(e.starts_with("state dir "), "{e}");
    drop(held);
    std::fs::write(cfg.state_dir.join("roll.json"), "{ not json").expect("corrupt");
    let e = start(&cfg).err().expect("corrupt");
    assert!(e.starts_with("roll record in "), "{e}");
    assert!(
        !cfg.socket.exists(),
        "nothing is bound when the record is bad"
    );

    let mut cfg = config("bad3", good, &api);
    let _live = UnixListener::bind(&cfg.socket).expect("someone listens");
    let e = start(&cfg).err().expect("live socket");
    assert!(e.starts_with("socket ") && e.contains("listens"), "{e}");

    cfg.token_file = file(&scratch("bad4"), "token", "short", 0o600);
    let e = build_ports(&cfg).err().expect("token");
    assert!(e.contains("32 to 4096") && !e.contains("short\n"), "{e}");
    assert!(start(&cfg).is_err());
}

/// Catches a queued roll that cannot be driven stopping the worker or going unreported: the
/// loop audits it and goes on to the next.
#[test]
fn the_driver_loop_audits_what_it_cannot_drive_and_goes_on() {
    let cfg = config(
        "loop",
        "st1 st1.farm.example storage KEY\n",
        &fake_api(vec![Some(http(200, "{}"))]),
    );
    let log = Log::default();
    let ports = Ports {
        safe: Box::new(FakeSafeToStop::new(&log)),
        installer: Box::new(FakeInstaller::new(&log)),
        health: Box::new(FakeHealth::new(&log)),
        clock: Box::new(FakeClock::new()),
    };
    let updater = Updater::open(
        crate::inventory::Inventory::load(&cfg.inventory, own_uid()).expect("inventory"),
        RollStore::open(&cfg.state_dir).expect("store"),
        ports,
        Timing::default(),
    )
    .expect("open");
    let req = crate::api::RollRequest::from_json(
        json!({"package": "kbf", "version": "2", "idempotency_key": "k"})
            .to_string()
            .as_bytes(),
    )
    .expect("request");
    let id = updater.start(&req).expect("start").roll.id;
    let (tx, rx) = std::sync::mpsc::channel();
    tx.send("roll-99".to_owned()).expect("send");
    tx.send(id).expect("send");
    drop(tx);
    let (audit, recent) = (Lines::default(), Recent::default());
    drive_loop(&updater, rx, &recent, &audit.sink());
    let lines = audit.all();
    assert_eq!(lines[0]["event"], "roll_not_driven");
    assert_eq!(lines[0]["roll_id"], "roll-99");
    assert_eq!(
        (lines[1]["event"].as_str(), lines[1]["state"].as_str()),
        (Some("roll_finished"), Some("done"))
    );
    assert_eq!(recent.list_except(None).len(), 1);
    assert_eq!(
        log.events()
            .iter()
            .filter(|e| matches!(e, Event::Install(..)))
            .count(),
        1
    );
}

/// Catches the process entry points: `--version`, a usage error for no flags, an unknown
/// argument and a missing flag (exit 2 with the usage), a start that fails (exit 1 with the
/// reason, and nothing bound), and a run that serves, answers, stops and exits 0 with its
/// socket removed.
#[test]
fn the_process_entry_points() {
    let (code, out) = run_with(&args(&["kbf-updater", "--version"]), |_| {
        panic!("not started")
    });
    assert_eq!((code, out.starts_with("kbf-updater 0.")), (0, true));
    for bad in [
        &["kbf-updater"][..],
        &["kbf-updater", "serve"],
        &["kbf-updater", "--socket", "/x"],
    ] {
        let (code, out) = run_with(&args(bad), |_| panic!("not started"));
        assert_eq!(code, 2, "{bad:?}");
        assert!(out.contains("usage: kbf-updater --version"), "{out}");
    }
    // Valid flags, but the inventory does not exist: exit 1 and the reason.
    let api = fake_api(vec![Some(http(200, "{}"))]);
    let cfg = config("entry", "st1 st1.farm.example storage KEY\n", &api);
    let a = [
        "kbf-updater",
        "--state-dir",
        cfg.state_dir.to_str().expect("utf8"),
        "--inventory",
        "/nonexistent/inventory",
        "--socket",
        cfg.socket.to_str().expect("utf8"),
        "--proxy-uid",
        "1",
        "--operator-api",
        "http://127.0.0.1:1",
        "--token-file",
        cfg.token_file.to_str().expect("utf8"),
    ];
    let (code, out) = run_with(&args(&a), |_| panic!("not started"));
    assert_eq!(code, 1);
    assert!(
        out.starts_with("kbf-updater: inventory /nonexistent/inventory"),
        "{out}"
    );
    assert!(!cfg.socket.exists());
    // A run that serves.
    let socket = cfg.socket.clone();
    let (code, out) = run_config(&cfg, |stopper| {
        let socket = socket.clone();
        std::thread::spawn(move || {
            assert_eq!(call(&socket, "GET", "/v1/updater/rolls", "").0, 200);
            stopper.stop();
        });
    });
    assert_eq!((code, out.as_str()), (0, ""));
    assert!(!cfg.socket.exists(), "the socket is removed on exit");
    stderr_sink()("{\"event\":\"test\"}");
}
