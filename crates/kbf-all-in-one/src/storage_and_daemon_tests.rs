//! The one command line of `kbf storage-and-daemon`.

use clap::error::ErrorKind;

use super::*;

/// The flags every command line here gives: the daemon's required ones.
const BASE: &[&str] = &[
    "storage-and-daemon",
    "--server=https://127.0.0.1:8981",
    "--ca-cert=ca.pem",
    "--cert=node.pem",
    "--key=node.key",
    "--node-id=node-1",
    "--driver=container",
];

fn flags(extra: &[&str]) -> Result<Flags, clap::Error> {
    let argv = BASE.iter().chain(extra);
    let matches = command().try_get_matches_from(argv)?;
    parse(&matches)
}

/// Catches a storage flag that never reaches the storage half (its listener, its
/// certificates, its stores), a daemon flag that never reaches the daemon half, and the
/// halves' shared thread flags held twice (clap's own check of the command panics on a
/// name held twice) or handed to the storage half, which sizes itself as `kbf storage`
/// does.
#[test]
fn each_flag_reaches_its_half_once() {
    command().debug_assert();
    let flags = flags(&[
        "--storage-listen=127.0.0.1:9982",
        "--storage-tls-cert=storage.pem",
        "--storage-tls-key=storage.key",
        "--storage-client-ca=ca.pem",
        "--cold-store=file:///var/kbf/cold",
        "--hot-store=file:///var/kbf/hot",
        "--cpu-threads=3",
        "--data-dir=/var/kbf",
    ])
    .expect("flags");
    assert_eq!(flags.storage.listen.to_string(), "127.0.0.1:9982");
    let tls = flags.storage.tls.expect("the three TLS flags");
    assert_eq!(
        tls.map(|p| p.display().to_string()),
        ["storage.pem", "storage.key", "ca.pem"]
    );
    assert_eq!(
        flags.storage.server.threads,
        kbf_threads::PoolArgs::default()
    );
    let daemon = format!("{:?}", flags.daemon);
    assert!(daemon.contains("cpu_threads: Some(3)"), "{daemon}");
    assert!(daemon.contains("\"/var/kbf\""), "{daemon}");
    let storage = format!("{:?}", flags.storage.server);
    assert!(storage.contains("\"/var/kbf/hot\""), "{storage}");
    assert!(storage.contains("\"/var/kbf/cold\""), "{storage}");
}

/// Catches a driver other than the container driver taken, `--cas` taken (a remote data
/// plane beside the storage it would reach over the network), and a flag of
/// `kbf-server` that `kbf storage` refuses (the REAPI listener, the state store) taken
/// here: each refused, named.
#[test]
fn other_drivers_cas_and_server_flags_are_refused() {
    for (extra, says) in [
        (&["--driver=native"][..], "--driver native"),
        (&["--driver=fake"][..], "--driver fake"),
        (&["--cas=https://127.0.0.1:8981"][..], "--cas"),
        (&["--listen=127.0.0.1:8980"][..], "refuses --listen"),
        (
            &["--state-store=sqlite:///var/kbf/s"][..],
            "refuses --state-store",
        ),
    ] {
        let refused = flags(extra).expect_err("refused");
        assert_eq!(refused.kind(), ErrorKind::ArgumentConflict, "{extra:?}");
        assert!(refused.to_string().contains(says), "{says}: {refused}");
    }
}
