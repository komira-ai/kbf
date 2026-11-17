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

/// Catches a storage half started on the store URI the frontend sent (defect: the stored
/// text is the flag again), which is stale after the farm re-points its store: whatever
/// the frontend sent, the flag is the word `registered`, so the node takes its store from
/// the registration at each start, and the mode's own parser reads it as that.
#[test]
fn a_storage_node_passes_registered() {
    let c = case("sent-cold-store");
    let config = config(&c.root, "[storage]\n");
    let node = enrolled(&["storage"], sent(&["storage"]));
    let argv = plan(&config, &node, &c.store, want(names::STORAGE)).expect("a plan");
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
    let argv = plan(&config, &node, &c.store, want(names::STORAGE)).expect("a plan");
    assert!(strings(&argv).contains(&"--cold-store=registered".to_owned()));
}

/// Catches a storage-only node that does not listen where the frontend said, does not
/// serve with the storage role's own certificate and the farm CA, does not register under
/// the address callers dial (its `kbf.toml` advertise, else its id and the frontend's
/// storage port).
#[test]
fn a_storage_only_node_runs_kbf_storage_registered_with_the_frontend() {
    let c = case("storage");
    let config = config(&c.root, "[storage]\n");
    let node = enrolled(&["storage"], sent(&["storage"]));
    let argv = plan(&config, &node, &c.store, want(names::STORAGE)).expect("a plan");
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
    let argv = plan(&config, &node, &c.store, want(names::STORAGE)).expect("a plan");
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

/// Catches a node enrolled for more roles than the mode needs (a host that ran a storage and a
/// daemon before the daemon was removed) started with anything but the storage flags: the plan is
/// the storage-only one, built from the storage role's certificate files.
#[test]
fn a_node_enrolled_for_a_daemon_role_too_runs_storage_on_the_storage_roles() {
    let c = case("both");
    let config = config(&c.root, "");
    let both = enrolled(&["storage", "daemon"], sent(&["storage", "daemon"]));
    let only = enrolled(&["storage"], sent(&["storage"]));
    let plan_of =
        |node| strings(&plan(&config, node, &c.store, want(names::STORAGE)).expect("a plan"));
    let text = plan_of(&both);
    assert_eq!(text, plan_of(&only));
    assert!(
        text.contains(&format!("--storage-tls-cert={}", pki(&c, "storage.pem"))),
        "{text:?}"
    );
    assert!(
        text.iter().all(|a| !a.contains("daemon.")),
        "no file of the daemon role: {text:?}"
    );
}

/// Catches a node that starts with a configuration it cannot run: no worker endpoint, or a
/// storage half with no usable port. Each says what is missing.
#[test]
fn a_configuration_the_node_cannot_run_is_refused_by_name() {
    let c = case("refused");
    let config = config(&c.root, "");
    let refuse = |mode: &str, held: &[&str], stored: StoredConfig| {
        let node = enrolled(held, stored);
        match plan(&config, &node, &c.store, want(mode)) {
            Err(Failure::Plan(message)) => message,
            other => panic!("planned: {other:?}"),
        }
    };

    let mut none = sent(&["storage"]);
    none.worker_endpoint.clear();
    assert!(refuse(names::STORAGE, &["storage"], none).contains("worker endpoint"));
    for port in [0, 70_000] {
        let mut stored = sent(&["storage"]);
        stored.storage_port = port;
        assert!(refuse(names::STORAGE, &["storage"], stored).contains("storage port"));
    }
}

/// Catches the mode table of enrollment: which roles each mode needs, and that the
/// frontend, which takes its files as flags, needs none.
#[test]
fn each_mode_needs_exactly_its_roles() {
    let roles =
        |mode: &str| Want::of(mode).map(|w| w.roles.iter().map(Role::name).collect::<Vec<_>>());
    assert_eq!(roles(names::STORAGE), Some(vec!["storage"]));
    assert_eq!(roles(names::FRONTEND), None);
    for removed in names::REMOVED_MODES {
        assert_eq!(roles(removed), None, "{removed}");
    }
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
    let (want, note) = resolve_mode(Some(names::STORAGE), None, path).expect("flag");
    assert_eq!((want.mode, note), (names::STORAGE, None));
    let (want, note) = resolve_mode(None, Some("storage"), path).expect("file");
    assert_eq!((want.mode, note), (names::STORAGE, None));
    let (want, note) = resolve_mode(Some(names::STORAGE), Some("storage"), path).expect("same");
    assert_eq!((want.mode, note), (names::STORAGE, None));
    let (want, note) = resolve_mode(Some(names::STORAGE), Some("frontend"), path).expect("both");
    assert_eq!(want.mode, names::STORAGE);
    let note = note.expect("the override is said");
    assert!(
        note.contains("--mode=storage") && note.contains("\"frontend\""),
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
    let e = resolve_mode(Some(names::FRONTEND), None, path).expect_err("flags only");
    assert!(
        matches!(&e, Failure::FlagsOnly(m) if m == names::FRONTEND),
        "{e}"
    );
    assert_eq!(e.exit_code(), 2);
    for removed in names::REMOVED_MODES {
        for flag in [None, Some(names::STORAGE)] {
            let e = resolve_mode(flag, Some(removed), path).expect_err("removed");
            assert!(matches!(e, Failure::RemovedMode { .. }), "{e}");
            assert_eq!(e.exit_code(), 2);
            assert_eq!(
                e.to_string(),
                format!("/etc/kbf/kbf.toml: mode = {removed:?}: {}", crate::REMOVED)
            );
        }
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

/// Catches the keys warning missing or worded differently for a storage node asked for keys,
/// and a warning for any other node.
#[test]
fn the_keys_warning_is_for_a_storage_node_asked_for_keys_alone() {
    let mut stored = sent(&["storage"]);
    stored.credential_mode = 1;
    assert_eq!(
        keys_warning(&stored),
        Some(
            "the frontend asks for object-store keys to be fetched; this build does not, and the \
             object-store client uses the credentials of this process"
        )
    );
    stored.credential_mode = 2;
    assert_eq!(keys_warning(&stored), None);
}

/// Catches a reload interval of 0 (the frontend sent none) passed to the mode as a flag: the
/// mode then keeps its own default.
#[test]
fn a_storage_node_told_no_reload_interval_passes_none() {
    let c = case("sent-no-reload");
    let config = config(&c.root, "[storage]\n");
    let mut stored = sent(&["storage"]);
    stored.tls_reload_interval_s = 0;
    let node = enrolled(&["storage"], stored);
    let argv = plan(&config, &node, &c.store, want(names::STORAGE)).expect("a plan");
    assert!(
        strings(&argv)
            .iter()
            .all(|a| !a.starts_with("--tls-reload-interval-secs")),
        "{:?}",
        strings(&argv)
    );
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
}
