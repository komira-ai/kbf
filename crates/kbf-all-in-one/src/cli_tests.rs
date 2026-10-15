//! The one command line of both halves.

use clap::error::ErrorKind;

use super::*;

/// The flags every command line here gives: the one the daemon half requires.
const BASE: &[&str] = &["kbf-all-in-one", "--node-id=node-1"];

fn flags(extra: &[&str]) -> Result<Flags, clap::Error> {
    let argv = BASE.iter().chain(extra);
    let matches = command().try_get_matches_from(argv)?;
    parse(&matches)
}

/// Catches a server flag that never reaches the server half, a daemon flag that never
/// reaches the daemon half, and the two halves' shared thread flags held twice (clap's
/// own check of the command panics on a name held twice) or handed to the server half,
/// which sizes itself as `kbf-server` does.
#[test]
fn each_flag_reaches_its_half_once() {
    command().debug_assert();
    let flags = flags(&[
        "--driver=container",
        "--listen=127.0.0.1:9980",
        "--cold-store=file:///var/kbf/cold",
        "--hot-store=file:///var/kbf/hot",
        "--cpu-threads=3",
        "--data-dir=/var/kbf",
    ])
    .expect("flags");
    assert_eq!(flags.server.listen.to_string(), "127.0.0.1:9980");
    assert_eq!(flags.server.threads, kbf_threads::PoolArgs::default());
    let daemon = format!("{:?}", flags.daemon);
    assert!(daemon.contains("cpu_threads: Some(3)"), "{daemon}");
    assert!(daemon.contains("\"/var/kbf\""), "{daemon}");
    let server = format!("{:?}", flags.server);
    assert!(server.contains("\"/var/kbf/hot\""), "{server}");
}

/// Catches a driver other than the container driver taken (the native drivers run
/// actions as the farm's own user, beside the server's state), whether alone or beside
/// the container driver, and `--cas` taken (a remote data plane in the same process as
/// its server). Each refusal says what to do instead.
#[test]
fn other_drivers_and_cas_are_refused() {
    for (extra, says) in [
        (&["--driver=native"][..], "--driver native"),
        (
            &["--driver=container", "--driver=fake"][..],
            "--driver fake",
        ),
        (
            &["--driver=container", "--cas=https://127.0.0.1:8981"][..],
            "--cas",
        ),
    ] {
        let refused = flags(extra).expect_err("refused");
        assert_eq!(refused.kind(), ErrorKind::ArgumentConflict, "{extra:?}");
        assert!(refused.to_string().contains(says), "{refused}");
    }
}

/// Catches a node identity, a dial or a worker listener flag taken (the daemon half
/// dials no server and presents no certificate, and the process binds no worker
/// listener), refused by one of the flag's own rules rather than by name (a
/// `--worker-tls-cert` given alone needs its key and client CA in `kbf-server`; a
/// `--server` given twice), or shown in `--help`. Each refusal names its flag and says
/// why; the daemon half's flags parse without any of them, and the server half keeps
/// its worker listener's default, which it does not bind.
#[test]
fn node_identity_and_worker_listener_flags_are_refused() {
    let parsed = flags(&["--driver=container"]).expect("no identity needed");
    let daemon = format!("{:?}", parsed.daemon);
    assert!(daemon.contains("servers: []"), "{daemon}");
    assert_eq!(parsed.server.worker_listen.to_string(), "127.0.0.1:8981");
    let dials = "opens its sessions through calls into the server half";
    let listens = "binds no worker listener";
    for (extra, says) in [
        (
            &["--server=https://127.0.0.1:8981"][..],
            ("--server", dials),
        ),
        (
            &["--server=https://a:1", "--server=https://b:1"][..],
            ("--server", dials),
        ),
        (&["--ca-cert=ca.pem"][..], ("--ca-cert", dials)),
        (&["--cert=node.pem"][..], ("--cert", dials)),
        (&["--key=node.key"][..], ("--key", dials)),
        (
            &["--tls-server-name=localhost"][..],
            ("--tls-server-name", dials),
        ),
        (
            &["--worker-listen=127.0.0.1:9981"][..],
            ("--worker-listen", listens),
        ),
        (
            &["--worker-tls-cert=server.pem"][..],
            ("--worker-tls-cert", listens),
        ),
        (
            &["--worker-tls-key=server.key"][..],
            ("--worker-tls-key", listens),
        ),
        (
            &["--worker-client-ca=ca.pem"][..],
            ("--worker-client-ca", listens),
        ),
        (
            &["--worker-deny-list=deny.json"][..],
            ("--worker-deny-list", listens),
        ),
    ] {
        let argv: Vec<&str> = ["--driver=container"]
            .iter()
            .chain(extra)
            .copied()
            .collect();
        let refused = flags(&argv).expect_err("refused");
        assert_eq!(refused.kind(), ErrorKind::ArgumentConflict, "{extra:?}");
        let text = refused.to_string();
        assert!(text.contains(&format!("{}: ", says.0)), "{text}");
        assert!(text.contains(says.1), "{text}");
    }
    let help = command().render_long_help().to_string();
    for (_, long, _) in REFUSED {
        assert!(!help.contains(&format!("--{long} ")), "--{long} in {help}");
    }
}
