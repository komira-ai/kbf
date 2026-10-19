//! Rules `storage`, `state-store` and `ports` read on their own: how a flag and a
//! variable are read, which `--state-store` values pass (a Postgres URI without a
//! password in the frontend's unit, and a SQLite file by an absolute path in the
//! all-in-one's), and which listeners share a port.

use std::path::Path;

use super::*;

fn unit(text: &str) -> UnitFile {
    UnitFile::parse(text).expect("a unit")
}

fn words(line: &str) -> Vec<String> {
    credential::words(line)
}

const KEY: &str = "/etc/kbf/s3.env";

/// The storage node's flags, as the template renders them.
const FULL: &str = "--cold-store=s3://kbf-cas?endpoint=http://127.0.0.1:9000 \
    --meta-dir=/var/lib/kbf-server/raft --storage-listen=192.0.2.10:8983 \
    --storage-tls-cert=/etc/kbf/storage.pem --storage-tls-key=/etc/kbf/storage.key \
    --storage-client-ca=/etc/kbf/ca.pem";

fn storage_unit(flags: &str, env: &str) -> UnitFile {
    unit(&format!(
        "[Service]\n{env}ExecStart=/usr/local/bin/kbf storage {flags}\n"
    ))
}

/// Catches a flag read only in its `=` form (clap takes `--name value` too), the first
/// of two values read where kbf reads the last, and a longer flag (`--state-store-x`)
/// taken for the one asked about.
#[test]
fn a_flag_is_read_in_both_forms_and_the_last_wins() {
    let w = words("kbf storage --meta-dir /a --meta-dir=/b --meta-dir-x=/c");
    assert_eq!(flag(&w, "meta-dir"), Some("/b"));
    let w = words("kbf storage --meta-dir=/a \"--meta-dir\" '/b'");
    assert_eq!(flag(&w, "meta-dir"), Some("/b"));
    assert_eq!(flag(&words("kbf --state-store-x=/c"), "state-store"), None);
    assert_eq!(flag(&words("kbf --state-store"), "state-store"), None);
}

/// Catches each of the six flags dropped from a storage node's unit (one red per flag
/// planted), a `--cold-store` read only in its `=` form, an `s3://` cold store without
/// the key file passed, and a `file://` cold store asked for one.
#[test]
fn a_storage_unit_names_its_stores_listener_and_tls_and_its_key() {
    let env = format!("EnvironmentFile={KEY}\n");
    let key = Path::new(KEY);
    assert_eq!(
        storage(&storage_unit(FULL, &env), key, false),
        Vec::<String>::new()
    );
    for f in STORAGE_FLAGS {
        let dropped: Vec<&str> = FULL
            .split_whitespace()
            .filter(|w| !w.starts_with(&format!("--{f}=")))
            .collect();
        let got = storage(&storage_unit(&dropped.join(" "), &env), key, false);
        assert_eq!(got.len(), 1, "--{f}: {got:?}");
        assert!(got[0].starts_with(&format!("gives no --{f}:")), "{got:?}");
    }
    let empty = FULL.replace("--meta-dir=/var/lib/kbf-server/raft", "--meta-dir=");
    assert_eq!(storage(&storage_unit(&empty, &env), key, false).len(), 1);
    let spaced = FULL.replace("--cold-store=", "--cold-store ");
    assert_eq!(
        storage(&storage_unit(&spaced, &env), key, false),
        Vec::<String>::new()
    );
    assert_eq!(
        storage(&storage_unit(FULL, ""), key, false),
        [
            "has an s3:// cold store and no EnvironmentFile=/etc/kbf/s3.env: kbf storage starts without the bucket's key pair and refuses"
        ]
    );
    // The key given through an optional, quoted file still counts.
    let quoted = format!("EnvironmentFile=-\"{KEY}\"\n");
    assert_eq!(
        storage(&storage_unit(FULL, &quoted), key, false),
        Vec::<String>::new()
    );
    let file = FULL.replace(
        "s3://kbf-cas?endpoint=http://127.0.0.1:9000",
        "file:///var/lib/kbf/cold",
    );
    assert_eq!(
        storage(&storage_unit(&file, ""), key, false),
        Vec::<String>::new()
    );
}

/// Catches a variable not found in either of systemd's spellings, an unclosed brace
/// or a bare `$` read as a name, and a name read past its last letter.
#[test]
fn a_state_store_value_names_its_variable() {
    let rows = [
        ("${KBF_STATE_STORE}", Some("KBF_STATE_STORE")),
        ("$KBF_STATE_STORE", Some("KBF_STATE_STORE")),
        ("x${A}y", Some("A")),
        ("$A/b", Some("A")),
        ("${A", None),
        ("$", None),
        ("${}", None),
        ("sqlite:///var/lib/kbf/state.db", None),
    ];
    for (value, want) in rows {
        assert_eq!(variable(value), want, "{value}");
    }
}

fn frontend(service: &str, flags: &str) -> UnitFile {
    unit(&format!(
        "[Service]\n{service}ExecStart=/usr/local/bin/kbf frontend {flags}\n"
    ))
}

const SQLITE_FRONTEND: &str = "--state-store is sqlite:// in a frontend unit: the frontend's state store is Postgres (postgres://<host>/<db>), and SQLite is dropped for it";
const PASSWORD: &str = "--state-store carries a password: kbf's command line is readable by every user on the node, and kbf refuses such a URI; give a socket or a passwordless URI";
const NOT_FRONTEND: &str =
    "--state-store is not postgres://<host>/<db>: the frontend's state store is Postgres";
const NOT_ANY: &str =
    "--state-store is neither postgres://<host>/<db> nor sqlite:///<absolute path>";
const BAD_PATH: &str = "--state-store is sqlite:// with an empty, relative or not normal path: give sqlite:///<absolute path>";

fn unit_of(mode: &str, flags: &str) -> UnitFile {
    unit(&format!(
        "[Service]\nExecStart=/usr/local/bin/kbf {mode} {flags}\n"
    ))
}

/// Catches a Postgres URI refused in the frontend's unit (either scheme, a socket form,
/// a host and port, in either flag form, quoted, or as the last of two values), a unit
/// with no `--state-store` failed, and a SQLite file passed there: the frontend's store
/// is Postgres and SQLite is dropped for it (planted mutants: the postgres branch
/// returning a refusal turns the first loop red; the frontend arm allowing sqlite turns
/// the second red). No message holds the value.
#[test]
fn the_frontends_state_store_is_postgres_and_sqlite_is_refused() {
    for flags in [
        "--state-store=postgres:///kbf?host=/run/postgresql",
        "--state-store postgres:///kbf?host=/run/postgresql",
        "\"--state-store=postgres:///kbf?host=/run/postgresql\"",
        "--state-store=postgresql://kbf@db.example:5432/kbf",
        "--state-store=postgres://db/kbf",
        "--state-store=sqlite:///s.db --state-store=postgres://db/kbf",
        "",
    ] {
        assert_eq!(
            state_store(&frontend("", flags), Mode::Frontend),
            Vec::<String>::new(),
            "{flags}"
        );
    }
    for flags in [
        "--state-store=sqlite:///var/lib/kbf/state.db",
        "--state-store sqlite:///var/lib/kbf/state.db",
        "\"--state-store=sqlite:///var/lib/kbf/state.db\"",
        "--state-store=postgres://db/kbf --state-store=sqlite:///var/lib/kbf/state.db",
        "--state-store=sqlite://relative/state.db",
        "--state-store=sqlite://",
    ] {
        let got = state_store(&frontend("", flags), Mode::Frontend);
        assert_eq!(got, [SQLITE_FRONTEND], "{flags}");
        assert!(!got[0].contains("state.db"));
    }
}

/// The all-in-one unit keeps its SQLite file until the farm is moved (its template says
/// why), so there the rule takes a SQLite file by an absolute path as well as Postgres,
/// and judges the path. Catches the frontend's refusal applied to every mode (the
/// all-in-one's own rendered unit would fail), and a bad SQLite path passed.
#[test]
fn the_all_in_one_unit_may_still_name_a_sqlite_file() {
    for flags in [
        "--state-store=sqlite:///var/lib/kbf/state.db",
        "--state-store=postgres:///kbf?host=/run/postgresql",
        "",
    ] {
        assert_eq!(
            state_store(&unit_of("all-in-one", flags), Mode::AllInOne),
            Vec::<String>::new(),
            "{flags}"
        );
    }
    for value in [
        "sqlite://",
        "sqlite:///",
        "sqlite://state.db",
        "sqlite://var/lib/kbf/state.db",
        "sqlite:////var/lib/kbf/state.db",
        "sqlite:///var/lib/../kbf/state.db",
    ] {
        assert_eq!(
            state_store(
                &unit_of("all-in-one", &format!("--state-store={value}")),
                Mode::AllInOne
            ),
            [BAD_PATH],
            "{value}"
        );
    }
}

/// Catches a password in the URI passed (as user information or as a `password`
/// parameter in any case, among other parameters), a URI without one refused for it
/// (a user name, a colon in a port or a query), and the secret echoed in the message.
#[test]
fn a_password_in_the_state_store_uri_fails_without_echo() {
    for value in [
        "postgres://kbf:hunter2@db/kbf",
        "postgresql://:hunter2@db/kbf",
        "postgres://db/kbf?password=hunter2",
        "postgres://db/kbf?sslmode=prefer&PASSWORD=hunter2",
        "postgres:///kbf?host=/run/postgresql&password=hunter2",
    ] {
        for mode in [Mode::Frontend, Mode::AllInOne] {
            let got = state_store(
                &unit_of(mode.name(), &format!("--state-store={value}")),
                mode,
            );
            assert_eq!(got, [PASSWORD], "{mode} {value}");
        }
    }
    for value in [
        "postgres://kbf@db:5432/kbf",
        "postgres://db/kbf?application_name=a:b",
        "postgres://db/kbf?passwordless=1",
        "postgres:///kbf?host=/run/postgresql",
    ] {
        assert_eq!(
            state_store(
                &frontend("", &format!("--state-store={value}")),
                Mode::Frontend
            ),
            Vec::<String>::new(),
            "{value}"
        );
    }
}

/// Catches a value of another scheme, or none, passed in either mode (or taken for
/// postgres or sqlite), and the message that names the wrong set of schemes for the mode.
#[test]
fn any_other_scheme_fails() {
    for value in [
        "",
        "state.db",
        "/var/lib/kbf/state.db",
        "mysql://kbf@db/kbf",
        "file:///var/lib/kbf/state.db",
        "sqlite:/var/lib/kbf/state.db",
        "postgres:/db/kbf",
        "postgres://",
        "SQLITE:///var/lib/kbf/state.db",
    ] {
        assert_eq!(
            state_store(
                &frontend("", &format!("--state-store={value}")),
                Mode::Frontend
            ),
            [NOT_FRONTEND],
            "{value}"
        );
        assert_eq!(
            state_store(
                &unit_of("all-in-one", &format!("--state-store={value}")),
                Mode::AllInOne
            ),
            [NOT_ANY],
            "{value}"
        );
    }
}

/// Catches `--state-store=${VAR}` passed (with or without a file to set it: the
/// expanded value is in kbf's argv either way, and the rule cannot read it), in either
/// spelling, inside an otherwise good SQLite value, and a variable taken for a scheme.
#[test]
fn a_state_store_that_names_a_variable_fails() {
    let filed = "EnvironmentFile=/etc/kbf/state-store.env\n";
    let refused = |var: &str| {
        format!(
            "--state-store names ${{{var}}}: systemd expands it into kbf's command line, which every user on the node can read in /proc, and the rule cannot read the store it names; write the URI itself"
        )
    };
    for (service, value, var) in [
        (filed, "${KBF_STATE_STORE}", "KBF_STATE_STORE"),
        ("", "$KBF_STATE_STORE", "KBF_STATE_STORE"),
        ("", "sqlite://${KBF_DATA_DIR}/state.db", "KBF_DATA_DIR"),
        ("", "postgres://$PGUSER@db/kbf", "PGUSER"),
    ] {
        assert_eq!(
            state_store(
                &frontend(service, &format!("--state-store={value}")),
                Mode::Frontend
            ),
            [refused(var)],
            "{value}"
        );
    }
}

/// Catches two listeners on one port passed (the first and the second, the first and
/// the last, the second form `--flag value`), and listeners on their own ports, or a
/// unit with none, failed.
#[test]
fn no_two_listeners_share_a_port() {
    let ports_of = |flags: &str| ports(&frontend("", flags));
    assert_eq!(
        ports_of("--listen=a:1 --worker-listen=a:2 --api-listen=127.0.0.1:3 --storage-listen=a:4"),
        Vec::<String>::new()
    );
    assert_eq!(ports_of(""), Vec::<String>::new());
    assert_eq!(
        ports_of("--listen=noport --api-listen=a:1"),
        Vec::<String>::new()
    );
    let tail = "give each listener its own (KBF_REAPI_PORT, KBF_WORKER_PORT, KBF_API_PORT, KBF_STORAGE_PORT)";
    assert_eq!(
        ports_of("--listen=a:1 --worker-listen [::1]:1"),
        [format!(
            "--listen and --worker-listen are both on port 1: {tail}"
        )]
    );
    assert_eq!(
        ports_of("--listen=a:1 --worker-listen=a:2 --api-listen=127.0.0.1:3 --storage-listen=b:1"),
        [format!(
            "--listen and --storage-listen are both on port 1: {tail}"
        )]
    );
}

/// Rule `meta-dir`: `--meta-dir` is not the data directory or under it, however the path
/// is written. Catches a metadata directory inside `--data-dir` passing (also spelled
/// with `.` or `..`), a sibling that only shares a name prefix (`/var/lib/kbf-server`
/// beside `/var/lib/kbf`) refused, and a unit with only one of the two flags refused.
#[test]
fn the_meta_dir_is_not_under_the_data_dir() {
    let unit = |meta: &str, data: &str| {
        UnitFile::parse(&format!(
            "[Service]\nExecStart=/usr/local/bin/kbf all-in-one {meta} {data}\n"
        ))
        .expect("a unit")
    };
    let under = |flags: &UnitFile| meta_dir(flags).len();
    let rows = [
        (
            "--meta-dir=/var/lib/kbf-server/raft",
            "--data-dir=/var/lib/kbf",
            0,
        ),
        ("--meta-dir=/var/lib/kbf/raft", "--data-dir=/var/lib/kbf", 1),
        ("--meta-dir=/var/lib/kbf", "--data-dir=/var/lib/kbf", 1),
        (
            "--meta-dir=/var/lib/kbf/./a/../raft",
            "--data-dir=/var/lib/kbf",
            1,
        ),
        (
            "--meta-dir=/var/lib/kbf/../raft",
            "--data-dir=/var/lib/kbf",
            0,
        ),
        (
            "--meta-dir=/var/lib/kbf/raft",
            "--data-dir=/var/lib/kbf/",
            1,
        ),
        ("--meta-dir=/var/lib/kbf/raft", "--verbose", 0),
        ("--verbose", "--data-dir=/var/lib/kbf", 0),
        // A relative path is left to rules `storage` and the parser: compared lexically
        // it would be "under" `.` or under a relative twin, and refused here for the
        // wrong reason.
        ("--meta-dir=var/lib/kbf/raft", "--data-dir=var/lib/kbf", 0),
        ("--meta-dir=raft", "--data-dir=.", 0),
        ("--meta-dir=/var/lib/kbf/raft", "--data-dir=var/lib/kbf", 0),
        ("--meta-dir=var/lib/kbf/raft", "--data-dir=/var/lib/kbf", 0),
    ];
    for (meta, data, want) in rows {
        assert_eq!(under(&unit(meta, data)), want, "{meta} {data}");
    }
}
