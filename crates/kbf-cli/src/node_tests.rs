//! What a start that reads `kbf.toml` runs for its mode: the command line of each mode,
//! built from `kbf.toml` and the frontend's configuration, and parsed by the mode's own
//! parser; how the mode is resolved; how long a start waits to try again.

use kbf_enroll::config::{Config, Host};
use kbf_enroll::store::{Node as Enrolled, Store, StoredCert, StoredConfig};

use crate::enroll::Want;

use super::*;
use crate::parse_from;

const FRONTEND: &str = "https://farm.example:8990";

/// A scratch directory (canonical path) removed when it drops. A unit test gets no
/// `CARGO_TARGET_TMPDIR`, so it is under the system's temporary directory.
fn scratch(name: &str) -> kbf_testkit::dir::TestDir {
    let temp = std::fs::canonicalize(std::env::temp_dir()).expect("canonical");
    kbf_testkit::dir::TestDir::under(temp, &format!("kbf-cli-node-{name}"))
}

/// A `kbf.toml` for node-02 with `extra` lines, its state under `root`.
fn config(root: &std::path::Path, extra: &str) -> Config {
    let text = format!(
        "[frontend]\nurl = \"{FRONTEND}\"\n[node]\nid = \"node-02\"\n{extra}\n\
         [dirs]\nstate = \"{}\"\n",
        root.join("state").display()
    );
    Config::parse(
        &text,
        &Host {
            hostname: "node-02".to_owned(),
            cpus: 8,
        },
    )
    .unwrap_or_else(|e| panic!("config: {e}"))
}

/// The frontend's configuration for a node granted `roles`, as a full answer carries it.
fn sent(roles: &[&str]) -> StoredConfig {
    StoredConfig {
        generation: 3,
        roles: roles.iter().map(|r| (*r).to_owned()).collect(),
        worker_endpoint: "https://farm.example:8981".to_owned(),
        cas_endpoint: String::new(),
        storage_port: 8983,
        cold_store: "s3://bucket/prefix/?endpoint=https://objects.example".to_owned(),
        credential_mode: 2,
        min_protocol: 1,
        tls_reload_interval_s: 30,
        frontend_url: String::new(),
    }
}

/// The record of a node holding a certificate for each of `held`.
fn enrolled(held: &[&str], config: StoredConfig) -> Enrolled {
    Enrolled {
        node_id: "node-02".to_owned(),
        pin: "ab".repeat(32),
        certs: held
            .iter()
            .map(|role| StoredCert {
                role: (*role).to_owned(),
                serial: "01".to_owned(),
                not_before: 0,
                not_after: 1,
            })
            .collect(),
        renew_after: 1,
        config,
    }
}

fn want(mode: &str) -> Want {
    Want::of(mode).expect("a mode that reads kbf.toml")
}

fn strings(argv: &[OsString]) -> Vec<String> {
    argv.iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

struct Case {
    store: Store,
    /// Last, so it drops after the store that has files in it.
    root: kbf_testkit::dir::TestDir,
}

fn case(name: &str) -> Case {
    let root = scratch(name);
    let store = Store::open(&root.join("state")).expect("state dir");
    Case { store, root }
}

fn pki(c: &Case, file: &str) -> String {
    c.root.join("state/pki").join(file).display().to_string()
}

fn state(c: &Case, rest: &str) -> String {
    c.root.join("state").join(rest).display().to_string()
}

/// Catches a daemon-only node started with anything but the flags a `kbf daemon` unit
/// carries (the server and the blob endpoint from the frontend, the three certificate
/// files of the daemon role, the node id, the data directory), a `--cas` that is not the
/// worker endpoint when the frontend names none, or the CPU reserve dropped.
#[test]
fn a_daemon_only_node_runs_kbf_daemon_with_the_frontends_endpoints() {
    let c = case("daemon");
    let config = config(&c.root, "[daemon]\nreserve_cpus = 2\n");
    let node = enrolled(&["daemon"], sent(&["daemon"]));
    let argv = plan(
        &config,
        &node,
        &c.store,
        Driver::Native,
        want(names::DAEMON),
    )
    .expect("a plan");
    assert_eq!(
        strings(&argv),
        [
            "kbf".to_owned(),
            "--mode=daemon".to_owned(),
            "--driver=native".to_owned(),
            "--server=https://farm.example:8981".to_owned(),
            "--cas=https://farm.example:8981".to_owned(),
            format!("--ca-cert={}", pki(&c, "ca.pem")),
            format!("--cert={}", pki(&c, "daemon.pem")),
            format!("--key={}", pki(&c, "daemon.key")),
            "--node-id=node-02".to_owned(),
            format!("--data-dir={}", state(&c, "data")),
            "--reserve-cpus=2".to_owned(),
        ]
    );
    assert!(matches!(parse_from(argv), Ok(Invocation::Daemon(_))));

    let mut stored = sent(&["daemon"]);
    stored.cas_endpoint = "https://blobs.example:8981".to_owned();
    let node = enrolled(&["daemon"], stored);
    let argv = plan(
        &config,
        &node,
        &c.store,
        Driver::Container,
        want(names::DAEMON),
    )
    .expect("a plan");
    let argv = strings(&argv);
    assert!(argv.contains(&"--cas=https://blobs.example:8981".to_owned()));
    assert!(argv.contains(&"--driver=container".to_owned()));
}

/// Catches a storage half started on the store URI the frontend sent (defect: the stored
/// text is the flag again), which is stale after the farm re-points its store: whatever
/// the frontend sent, the flag is the word `registered`, so the node takes its store from
/// the registration at each start, and the mode's own parser reads it as that.
#[test]
fn kbf_node_passes_registered() {
    let c = case("sent-cold-store");
    let config = config(&c.root, "[storage]\n");
    let node = enrolled(&["storage"], sent(&["storage"]));
    let argv = plan(
        &config,
        &node,
        &c.store,
        Driver::Container,
        want(names::STORAGE),
    )
    .expect("a plan");
    let text = strings(&argv);
    assert!(
        text.contains(&"--cold-store=registered".to_owned()),
        "{text:?}"
    );
    assert!(
        text.iter().all(|a| !a.contains("objects.example")),
        "the stored URI is not passed: {text:?}"
    );
    let Ok(Invocation::Storage(flags)) = parse_from(argv) else {
        panic!("not a storage invocation");
    };
    assert_eq!(
        flags.server.cold_store.map(|store| store.to_string()),
        Some("registered".to_owned())
    );
}

/// Catches a node refused because the frontend sent it no store (defect: the missing
/// store is a plan error): the store is the registration's, which the node waits for, so a
/// configuration with no cold store still plans, with the same flag.
#[test]
fn a_storage_node_told_no_cold_store_still_runs_registered() {
    let c = case("sent-no-cold-store");
    let config = config(&c.root, "[storage]\n");
    let mut stored = sent(&["storage"]);
    stored.cold_store.clear();
    let node = enrolled(&["storage"], stored);
    let argv = plan(
        &config,
        &node,
        &c.store,
        Driver::Container,
        want(names::STORAGE),
    )
    .expect("a plan");
    assert!(strings(&argv).contains(&"--cold-store=registered".to_owned()));
}

/// Catches a storage-only node that does not listen where the frontend said, does not
/// serve with the storage role's own certificate and the farm CA, does not register under
/// the address callers dial (its `kbf.toml` advertise, else its id and the frontend's
/// storage port), or starts a daemon.
#[test]
fn a_storage_only_node_runs_kbf_storage_registered_with_the_frontend() {
    let c = case("storage");
    let config = config(&c.root, "[storage]\n");
    let node = enrolled(&["storage"], sent(&["storage"]));
    let argv = plan(
        &config,
        &node,
        &c.store,
        Driver::Container,
        want(names::STORAGE),
    )
    .expect("a plan");
    assert_eq!(
        strings(&argv),
        [
            "kbf".to_owned(),
            "--mode=storage".to_owned(),
            "--cold-store=registered".to_owned(),
            format!("--hot-store=file://{}", state(&c, "hot")),
            format!("--meta-dir={}", state(&c, "meta")),
            "--storage-listen=0.0.0.0:8983".to_owned(),
            format!("--storage-tls-cert={}", pki(&c, "storage.pem")),
            format!("--storage-tls-key={}", pki(&c, "storage.key")),
            format!("--storage-client-ca={}", pki(&c, "ca.pem")),
            "--frontend=https://farm.example:8981".to_owned(),
            "--advertise-addr=node-02:8983".to_owned(),
            "--tls-reload-interval-secs=30".to_owned(),
        ]
    );
    assert!(matches!(parse_from(argv), Ok(Invocation::Storage(_))));

    let config = config_with_advertise(&c);
    let argv = plan(
        &config,
        &node,
        &c.store,
        Driver::Container,
        want(names::STORAGE),
    )
    .expect("a plan");
    let argv = strings(&argv);
    assert!(argv.contains(&"--advertise-addr=node-02.farm.example:9000".to_owned()));
    assert!(argv.contains(&"--storage-zone=rack-a".to_owned()));
}

fn config_with_advertise(c: &Case) -> Config {
    config(
        &c.root,
        "advertise = \"node-02.farm.example:9000\"\nzone = \"rack-a\"\n[storage]\n",
    )
}

/// Catches both roles started as two processes, or as `kbf storage-and-daemon` without
/// the storage flags or with the daemon's `--cas` (which that mode refuses: its daemon
/// reads the node's storage in-process), a reload interval the frontend turned off
/// still passed, and either half's certificate files being another role's.
#[test]
fn a_node_with_both_roles_runs_the_converged_mode() {
    let c = case("both");
    let config = config(&c.root, "");
    let mut stored = sent(&["storage", "daemon"]);
    stored.tls_reload_interval_s = 0;
    let node = enrolled(&["storage", "daemon"], stored);
    let argv = plan(
        &config,
        &node,
        &c.store,
        Driver::Container,
        want(names::STORAGE_AND_DAEMON),
    )
    .expect("a plan");
    let text = strings(&argv);
    assert_eq!(text[1], "--mode=storage-and-daemon");
    assert!(text.iter().any(|a| a.starts_with("--storage-listen=")));
    // The certificate files of each half are the enrolled node's own, by role: the storage
    // half serves with the storage role's pair and the farm CA, the daemon half dials with
    // the daemon role's.
    for (flag, file) in [
        ("--storage-tls-cert", "storage.pem"),
        ("--storage-tls-key", "storage.key"),
        ("--storage-client-ca", "ca.pem"),
        ("--ca-cert", "ca.pem"),
        ("--cert", "daemon.pem"),
        ("--key", "daemon.key"),
    ] {
        assert!(
            text.contains(&format!("{flag}={}", pki(&c, file))),
            "{flag} is not {file}: {text:?}"
        );
    }
    assert!(!text.iter().any(|a| a.starts_with("--cas=")), "{text:?}");
    assert!(
        !text
            .iter()
            .any(|a| a.starts_with("--tls-reload-interval-secs")),
        "{text:?}"
    );
    assert!(matches!(
        parse_from(argv),
        Ok(Invocation::StorageAndDaemon(_))
    ));
}

/// Catches a node that starts with a configuration it cannot run: no worker endpoint, or a
/// storage half with no usable port. Each says what is missing.
#[test]
fn a_configuration_the_node_cannot_run_is_refused_by_name() {
    let c = case("refused");
    let config = config(&c.root, "");
    let refuse = |mode: &str, held: &[&str], stored: StoredConfig| {
        let node = enrolled(held, stored);
        match plan(&config, &node, &c.store, Driver::Container, want(mode)) {
            Err(Failure::Plan(message)) => message,
            other => panic!("planned: {other:?}"),
        }
    };

    let mut none = sent(&["daemon"]);
    none.worker_endpoint.clear();
    assert!(refuse(names::DAEMON, &["daemon"], none).contains("worker endpoint"));
    for port in [0, 70_000] {
        let mut stored = sent(&["storage"]);
        stored.storage_port = port;
        assert!(refuse(names::STORAGE, &["storage"], stored).contains("storage port"));
    }
}

/// Catches the mode table of enrollment: which roles each mode needs, and that the
/// frontend and all-in-one, which take their files as flags, need none.
#[test]
fn each_mode_needs_exactly_its_roles() {
    let roles =
        |mode: &str| Want::of(mode).map(|w| w.roles.iter().map(Role::name).collect::<Vec<_>>());
    assert_eq!(roles(names::STORAGE), Some(vec!["storage"]));
    assert_eq!(roles(names::DAEMON), Some(vec!["daemon"]));
    assert_eq!(
        roles(names::STORAGE_AND_DAEMON),
        Some(vec!["storage", "daemon"])
    );
    assert_eq!(roles(names::FRONTEND), None);
    assert_eq!(roles(names::ALL_IN_ONE), None);
    assert_eq!(roles("node"), None);
    for mode in names::MODES {
        assert_eq!(
            Want::of(mode.name).is_some(),
            names::is_registered(mode.name),
            "{}",
            mode.name
        );
    }
}

/// Catches the order of the mode's sources: the flag beats the file (and the start says
/// so), the file is used when there is no flag, neither is an error naming both, a name
/// in the file that is not a mode is refused, and a mode that takes flags only is
/// refused with how to start it.
#[test]
fn the_flag_beats_the_file_and_neither_is_an_error() {
    let path = std::path::Path::new("/etc/kbf/kbf.toml");
    let (want, note) = resolve_mode(Some(names::DAEMON), None, path).expect("flag");
    assert_eq!((want.mode, note), (names::DAEMON, None));
    let (want, note) = resolve_mode(None, Some("storage"), path).expect("file");
    assert_eq!((want.mode, note), (names::STORAGE, None));
    let (want, note) = resolve_mode(Some(names::STORAGE), Some("storage"), path).expect("same");
    assert_eq!((want.mode, note), (names::STORAGE, None));
    let (want, note) = resolve_mode(Some(names::DAEMON), Some("storage"), path).expect("both");
    assert_eq!(want.mode, names::DAEMON);
    let note = note.expect("the override is said");
    assert!(
        note.contains("--mode=daemon") && note.contains("\"storage\""),
        "{note}"
    );

    let none = resolve_mode(None, None, path).expect_err("no mode");
    let text = none.to_string();
    assert!(matches!(none, Failure::NoMode { .. }), "{text}");
    assert!(
        text.contains("--mode") && text.contains("mode = ") && text.contains("kbf.toml"),
        "{text}"
    );
    assert_eq!(none.exit_code(), 2);

    let bad = resolve_mode(None, Some("node"), path).expect_err("not a mode");
    assert!(matches!(bad, Failure::BadMode { .. }), "{bad}");
    assert!(bad.to_string().contains("\"node\""), "{bad}");
    for flags_only in [names::FRONTEND, names::ALL_IN_ONE] {
        let e = resolve_mode(Some(flags_only), None, path).expect_err("flags only");
        assert!(
            matches!(&e, Failure::FlagsOnly(m) if m == flags_only),
            "{e}"
        );
        assert_eq!(e.exit_code(), 2);
    }
}

/// Catches a retry that spins (no wait), never grows, or grows without a cap: 5 seconds,
/// doubling, at most 5 minutes, and an attempt count that overflows still caps.
#[test]
fn a_start_waits_longer_each_time_up_to_five_minutes() {
    let secs = |attempt| retry_wait(attempt).as_secs();
    assert_eq!(
        [0, 1, 2, 3, 4, 5, 6, 7].map(secs),
        [5, 10, 20, 40, 80, 160, 300, 300]
    );
    assert_eq!(secs(63), 300);
    assert_eq!(secs(u32::MAX), 300);
}

/// Catches the warning about object-store keys appearing for a node that holds no storage
/// role, or for a frontend that does not ask for keys (the credentials are the process's).
#[test]
fn only_a_storage_node_asked_for_keys_is_told_they_are_not_fetched() {
    let with = |roles: &[&str], mode: i32| {
        let mut stored = sent(roles);
        stored.credential_mode = mode;
        wants_keys(&stored)
    };
    assert!(with(&["storage"], 1));
    assert!(with(&["storage", "daemon"], 1));
    assert!(!with(&["daemon"], 1));
    assert!(!with(&["storage"], 2));
    assert!(!with(&["storage"], 0));
}

/// Catches a loop that spins (a wait of nothing, or of a time already past) or that sleeps
/// longer than its cap (a clock that jumped, a machine that slept).
#[test]
fn a_nap_is_at_least_a_second_and_at_most_the_cap() {
    let cap = Duration::from_secs(3600);
    for (wait, want) in [
        (-5, 1),
        (0, 1),
        (1, 1),
        (300, 300),
        (3600, 3600),
        (302_400, 3600),
    ] {
        assert_eq!(nap(wait, cap), Duration::from_secs(want), "{wait}");
    }
    assert_eq!(
        nap(300, Duration::from_millis(10)),
        Duration::from_millis(10)
    );
}

/// Catches the system's pause that returns at once (a retry loop that spins on a down
/// frontend), that gives up (`false` ends the start with the failure it was retrying), or
/// a clock that is not the system's: the pause waits at least what it was given and says
/// to go on, and the clock reads the unix time between two reads of the system's.
#[test]
fn the_system_env_sleeps_what_it_is_told_and_reads_the_clock() {
    let env = Env::system();
    let wait = Duration::from_millis(20);
    let began = std::time::Instant::now();
    assert!((env.pause)(wait));
    assert!(began.elapsed() >= wait, "{:?}", began.elapsed());

    let unix = |at: std::time::SystemTime| {
        i64::try_from(
            at.duration_since(std::time::UNIX_EPOCH)
                .expect("after the epoch")
                .as_secs(),
        )
        .expect("fits")
    };
    let before = unix(std::time::SystemTime::now());
    let read = (env.now)();
    let after = unix(std::time::SystemTime::now());
    assert!((before..=after).contains(&read), "{before} {read} {after}");
    assert!(!env.host.hostname.is_empty());
    assert!(env.host.cpus >= 1);
}
