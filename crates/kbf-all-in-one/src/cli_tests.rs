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

/// Catches an all-in-one that keeps no durable state when `--state-store` is left out
/// (the scheduler would run in memory and a restart lose its queue), one whose default is
/// still a SQLite file in the data directory, one that puts the default anywhere but the
/// local Postgres socket whatever `--data-dir` says, and one that replaces a store the
/// operator named.
#[test]
fn the_state_store_defaults_to_the_local_postgres_socket() {
    let store = |extra: &[&str]| {
        let given = [&["--driver=container"], extra].concat();
        flags(&given).expect("flags").server.state_store
    };
    let local = Some(kbf_server::StateStoreUri::local_postgres());
    assert_eq!(store(&[]), local);
    assert_eq!(store(&["--data-dir=/srv/kbf"]), local);
    assert_eq!(
        store(&[
            "--data-dir=/srv/kbf",
            "--state-store=sqlite:///elsewhere/s.db"
        ]),
        Some(kbf_server::StateStoreUri::Sqlite("/elsewhere/s.db".into()))
    );
    assert_eq!(
        store(&["--state-store=postgres:///other?host=/run/postgresql"]),
        Some(
            "postgres:///other?host=/run/postgresql"
                .parse()
                .expect("parses")
        )
    );
}

/// Catches an all-in-one that takes a Postgres URL with a password (the command line is
/// world-readable) or one it cannot use, and a value of another kind.
#[test]
fn a_password_an_unusable_url_and_another_kind_of_value_are_refused() {
    for (value, says) in [
        (
            "postgres://kbf:hunter2@db.example/kbf",
            "must carry no password",
        ),
        ("postgres://", "needs a URL with a host"),
        ("file:///x", "only postgres:///"),
    ] {
        let arg = format!("--state-store={value}");
        let refused = flags(&["--driver=container", &arg]).expect_err(value);
        let said = refused.to_string();
        assert!(said.contains(says), "{value}: {said}");
        assert!(!said.contains("hunter2"), "{value}: {said}");
    }
}

/// Catches a refused `--state-store` that quotes its value back: clap echoes the value of
/// a parser's refusal (`invalid value '...'`), the command line is world-readable, and
/// the error goes to stderr and the journal. Each value holds a password in a form a
/// mistyped URL takes: user information, no `://`, a scheme position, and a parameter.
#[test]
fn a_refused_state_store_never_echoes_its_value() {
    for value in [
        "postgres://kbf:hunter2@db/x",
        "postgres:kbf:hunter2@db/x",
        "postgres:kbf:hunter2@db://x",
        "postgres://db/x?user=kbf&password=hunter2",
        "sqlite://hunter2",
    ] {
        let arg = format!("--state-store={value}");
        let refused = flags(&["--driver=container", &arg]).expect_err(value);
        let said = refused.to_string();
        for secret in ["hunter2", "kbf:", "kbf@", "password="] {
            assert!(!said.contains(secret), "{value}: {said}");
        }
        assert!(said.contains("--state-store: "), "{value}: {said}");
    }
}
