//! The one command line of both halves.

use clap::error::ErrorKind;

use super::*;

/// The flags every command line here gives: the daemon's required ones.
const BASE: &[&str] = &[
    "kbf-all-in-one",
    "--server=https://127.0.0.1:8981",
    "--ca-cert=ca.pem",
    "--cert=node.pem",
    "--key=node.key",
    "--node-id=node-1",
];

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
        "--worker-listen=127.0.0.1:9981",
        "--listen=127.0.0.1:9980",
        "--cold-store=file:///var/kbf/cold",
        "--hot-store=file:///var/kbf/hot",
        "--cpu-threads=3",
        "--data-dir=/var/kbf",
    ])
    .expect("flags");
    assert_eq!(flags.server.worker_listen.to_string(), "127.0.0.1:9981");
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
