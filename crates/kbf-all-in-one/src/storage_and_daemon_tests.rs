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

/// Catches the daemon half holding where the cold store is (its URI, which names the
/// bucket and the endpoint its credential opens) or any store flag: the cold store is
/// the storage half's alone, and the daemon reaches blobs only through the in-process
/// storage. The cold URI given must be in the storage half's flags and in nothing the
/// daemon half holds, and the daemon's command line (`kbf-daemon`'s) takes no store flag.
#[test]
fn the_daemon_half_holds_no_cold_store() {
    let cold = "s3://private-bucket/kbf/?endpoint=http://127.0.0.1:1";
    let flags = flags(&[&format!("--cold-store={cold}")]).expect("flags");
    let storage = format!("{:?}", flags.storage.server);
    assert!(storage.contains("private-bucket"), "{storage}");
    let daemon = format!("{:?}", flags.daemon);
    assert!(!daemon.contains("private-bucket"), "{daemon}");
    for arg in kbf_node::Cli::command().get_arguments() {
        let id = arg.get_id().as_str();
        assert!(
            !["cold_store", "hot_store", "meta_dir"].contains(&id),
            "the daemon's own flags include a store flag: {id}"
        );
    }
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

/// Catches a half started from flags other than the ones given the one cache (the
/// daemon half handed a copy made before the sharing, or the storage half): with the
/// hot store on the data directory's disk, both halves of the flags `serve` takes hold
/// the same cache.
#[cfg(target_os = "linux")]
#[test]
fn both_halves_start_with_the_one_cache() {
    let dir = crate::one_cache::under_target("prepared-storage-and-daemon");
    let at = |name: &str| dir.join(name).display().to_string();
    let hot = format!("--hot-store=file://{}", at("hot"));
    let data = format!("--data-dir={}", at("data"));
    let flags = prepared(&flags(&[&hot, &data]).expect("flags"));
    let local = flags.storage.server.local_cache.as_ref();
    let local = local.expect("the storage half's");
    assert!(crate::one_cache::daemon_holds(local, &flags.daemon));
}
