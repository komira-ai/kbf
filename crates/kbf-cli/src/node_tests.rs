//! What `kbf node` runs for the roles it was granted: the command line of each mode,
//! built from `kbf.toml` and the frontend's configuration, and parsed by the mode's own
//! parser.

use kbf_enroll::config::{Config, Host};
use kbf_enroll::store::{Node as Enrolled, Store, StoredCert, StoredConfig};

use super::*;

const FRONTEND: &str = "https://farm.example:8990";

/// A scratch directory of this test run.
fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("kbf-cli-node-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    std::fs::canonicalize(&dir).expect("canonical")
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

fn strings(argv: &[OsString]) -> Vec<String> {
    argv.iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

struct Case {
    root: std::path::PathBuf,
    store: Store,
}

fn case(name: &str) -> Case {
    let root = scratch(name);
    let store = Store::open(&root.join("state")).expect("state dir");
    Case { root, store }
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
/// worker endpoint when the frontend names none, or labels and the CPU reserve dropped.
#[test]
fn a_daemon_only_node_runs_kbf_daemon_with_the_frontends_endpoints() {
    let c = case("daemon");
    let config = config(&c.root, "[daemon]\nreserve_cpus = 2\n");
    let node = enrolled(&["daemon"], sent(&["daemon"]));
    let labels = ["pool=mac".to_owned(), "rack=r2".to_owned()];
    let argv = plan(&config, &node, &c.store, Driver::Native, &labels).expect("a plan");
    assert_eq!(
        strings(&argv),
        [
            "kbf".to_owned(),
            "daemon".to_owned(),
            "--driver=native".to_owned(),
            "--server=https://farm.example:8981".to_owned(),
            "--cas=https://farm.example:8981".to_owned(),
            format!("--ca-cert={}", pki(&c, "ca.pem")),
            format!("--cert={}", pki(&c, "daemon.pem")),
            format!("--key={}", pki(&c, "daemon.key")),
            "--node-id=node-02".to_owned(),
            format!("--data-dir={}", state(&c, "data")),
            "--reserve-cpus=2".to_owned(),
            "--label=pool=mac".to_owned(),
            "--label=rack=r2".to_owned(),
        ]
    );
    assert!(matches!(parse_from(argv), Ok(Invocation::Daemon(_))));

    let mut stored = sent(&["daemon"]);
    stored.cas_endpoint = "https://blobs.example:8981".to_owned();
    let node = enrolled(&["daemon"], stored);
    let argv = plan(&config, &node, &c.store, Driver::Container, &[]).expect("a plan");
    let argv = strings(&argv);
    assert!(argv.contains(&"--cas=https://blobs.example:8981".to_owned()));
    assert!(argv.contains(&"--driver=container".to_owned()));
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
    let argv = plan(&config, &node, &c.store, Driver::Container, &[]).expect("a plan");
    assert_eq!(
        strings(&argv),
        [
            "kbf".to_owned(),
            "storage".to_owned(),
            "--cold-store=s3://bucket/prefix/?endpoint=https://objects.example".to_owned(),
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
    let argv = plan(&config, &node, &c.store, Driver::Container, &[]).expect("a plan");
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
/// reads the node's storage in-process), and a reload interval the frontend turned off
/// still passed.
#[test]
fn a_node_with_both_roles_runs_the_converged_mode() {
    let c = case("both");
    let config = config(&c.root, "");
    let mut stored = sent(&["storage", "daemon"]);
    stored.tls_reload_interval_s = 0;
    let node = enrolled(&["storage", "daemon"], stored);
    let argv = plan(&config, &node, &c.store, Driver::Container, &[]).expect("a plan");
    let text = strings(&argv);
    assert_eq!(text[1], "storage-and-daemon");
    assert!(text.iter().any(|a| a.starts_with("--storage-listen=")));
    assert!(text.iter().any(|a| a.starts_with("--cert=")));
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

/// Catches a node that starts with a configuration it cannot run: no worker endpoint, a
/// storage role with no usable port or no cold store, roles the node cannot name, and a
/// role the frontend granted that the node holds no certificate for (it would start a
/// mode whose certificate files do not exist). Each says what is missing.
#[test]
fn a_configuration_the_node_cannot_run_is_refused_by_name() {
    let c = case("refused");
    let config = config(&c.root, "");
    let refuse = |held: &[&str], stored: StoredConfig| {
        let node = enrolled(held, stored);
        match plan(&config, &node, &c.store, Driver::Container, &[]) {
            Err(Failure::Plan(message)) => message,
            other => panic!("planned: {other:?}"),
        }
    };

    let mut none = sent(&["daemon"]);
    none.worker_endpoint.clear();
    assert!(refuse(&["daemon"], none).contains("worker endpoint"));
    for port in [0, 70_000] {
        let mut stored = sent(&["storage"]);
        stored.storage_port = port;
        assert!(refuse(&["storage"], stored).contains("storage port"));
    }
    let mut stored = sent(&["storage"]);
    stored.cold_store.clear();
    assert!(refuse(&["storage"], stored).contains("cold store"));

    assert!(refuse(&["daemon"], sent(&[])).contains("no usable roles"));
    assert!(refuse(&["daemon"], sent(&["frontend"])).contains("no usable roles"));
    assert!(refuse(&["daemon"], sent(&["storage", "daemon"])).contains("storage role"));
}

/// Catches the mode table: which roles run which mode.
#[test]
fn the_roles_pick_the_mode() {
    let set = |names: &[&str]| RoleSet::from_names(names.iter().copied()).expect("roles");
    assert_eq!(mode_of(set(&["daemon"])), names::DAEMON);
    assert_eq!(mode_of(set(&["storage"])), names::STORAGE);
    assert_eq!(
        mode_of(set(&["daemon", "storage"])),
        names::STORAGE_AND_DAEMON
    );
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
