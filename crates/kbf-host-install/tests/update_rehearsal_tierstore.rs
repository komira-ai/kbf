//! The update rehearsal of tierstore as a service of the coordinator: the coordinator of this
//! checkout, given a services file that holds kbf's own entry and the shipped
//! `tierstore.services.toml`, is driven by `cargo xtask deploy-farm --service tierstore`
//! (this checkout's) as the release workflow's gamma stage runs it, and rolls tierstore over
//! the hosts of the storage role. A change to the coordinator, the services file, the
//! heartbeat wire or the deploy verb that breaks the rollout of a service goes red here.
//!
//! What runs: the real coordinator (`kbf-coordinator`, built by the job) with a throwaway farm
//! CA and the allow-list of the release workflow's OIDC token, widened to `tierstore`, whose
//! issuer is the rehearsal's stand-in (`rehearsal/issuer.rs`); `xtask deploy-farm`; and six
//! stand-in storage hosts (`rehearsal/fleet.rs`) that heartbeat over mutual TLS with
//! certificates of the farm CA and announce `tierstore`, as the agent does with `--service`.
//! Six is the farm the shipped definition is written for (`min_up = 4`: one host updating, at least four others up).
//!
//! What it proves: with the hosts at version 1.0.0, `deploy-farm` of 1.0.0 ends 0 at once; of
//! 2.0.0 it ends 0 once the coordinator's convergence for `?service=tierstore` says every
//! host runs it, and every host does; kbf's own desired state is the same before and after. A
//! third version that every host refuses leaves `deploy-farm` ending with an error, and the
//! hosts where they were. The wait is `deploy-farm`'s own (`--deadline-min`, `--stall-min`);
//! the test has no clock of its own.
//!
//! What is not covered: the hosts install nothing (the real agent's A/B of a second service
//! needs a tierstore release and a helper for it), and the issuer is a stand-in for GitHub's.
//!
//! It needs Linux and the coordinator and `xtask` binaries built beside the test, so it is
//! `#[ignore]`d and runs through `cargo xtask sudo-test -p kbf-host-install --test
//! update_rehearsal_tierstore`.

#![cfg(target_os = "linux")]

mod rehearsal;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use rehearsal::farm::{self, PATIENCE};
use rehearsal::fleet::{Fleet, Host, SERVICE};
use rehearsal::issuer::{Issuer, REQUEST_TOKEN};
use rehearsal::{WORK, out_dir};
use serde_json::Value;

/// kbf's own entry of a services file and the entry the deploy document tells an operator to
/// append: the two files the coordinator ships.
const KBF_ENTRY: &str = include_str!("../../kbf-coordinator/src/kbf.services.toml");
const TIERSTORE_ENTRY: &str = include_str!("../../kbf-coordinator/src/tierstore.services.toml");

/// The storage hosts of the farm.
const NODES: [&str; 6] = [
    "storage-1",
    "storage-2",
    "storage-3",
    "storage-4",
    "storage-5",
    "storage-6",
];

/// Where cargo put the programs this checkout builds: beside the directory of test binaries.
fn built(name: &str) -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let path = exe.parent().unwrap().parent().unwrap().join(name);
    assert!(
        path.is_file(),
        "{} is not built: the job builds `{name}` before the test",
        path.display()
    );
    path
}

/// The farm of the rehearsal: the coordinator, the hosts, and what `deploy-farm` is given.
struct Farm {
    coordinator: farm::Proc,
    fleet: Fleet,
    ca_file: PathBuf,
    ca_pem: String,
    control_url: String,
    trust: PathBuf,
    issuer: Issuer,
    xtask: PathBuf,
}

impl Farm {
    /// Every host beats once, and the coordinator is still there.
    fn beat(&mut self, refuses: Option<&str>) {
        self.coordinator.alive();
        self.fleet.round(&self.ca_file, refuses);
    }

    /// Runs `deploy-farm` of `version` for tierstore while the hosts beat, and returns how it
    /// ended with its output.
    fn deploy(&mut self, name: &str, version: &str, refuses: Option<&str>) -> (ExitStatus, String) {
        let log = out_dir().join(format!("{name}.log"));
        let file = fs::File::create(&log).unwrap();
        let mut child: Child = Command::new(&self.xtask)
            .args(["deploy-farm", "--url", &self.control_url])
            .args(["--ca-pem", &self.ca_pem])
            .args(["--service", SERVICE, "--version", version])
            .args([
                "--every-secs",
                "5",
                "--stall-min",
                "15",
                "--deadline-min",
                "25",
            ])
            .env("ACTIONS_ID_TOKEN_REQUEST_URL", self.issuer.token_url())
            .env("ACTIONS_ID_TOKEN_REQUEST_TOKEN", REQUEST_TOKEN)
            .env("SSL_CERT_FILE", &self.trust)
            .stdin(Stdio::null())
            .stdout(file.try_clone().unwrap())
            .stderr(file)
            .spawn()
            .unwrap_or_else(|e| panic!("{name}: deploy-farm did not start: {e}"));
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            self.beat(refuses);
            sleep(Duration::from_secs(1));
        };
        (status, fs::read_to_string(&log).unwrap())
    }

    fn status(&self) -> Value {
        farm::status(&self.ca_file)
    }
}

#[test]
#[ignore = "needs the built coordinator and xtask: cargo xtask sudo-test"]
fn deploy_farm_rolls_tierstore_over_the_storage_hosts_and_kbf_is_untouched() {
    let coordinator_binary = built("kbf-coordinator");
    let xtask = built("xtask");

    // The farm CA and the certificates it issues.
    fs::create_dir_all(WORK).unwrap();
    let farm_dir = Path::new(WORK).join("tierstore-farm");
    fs::create_dir_all(&farm_dir).unwrap();
    fs::set_permissions(&farm_dir, fs::Permissions::from_mode(0o755)).unwrap();
    let pki = farm::Pki::new(&farm_dir.join("ca"));
    let ca_pem = pki.ca_pem();
    let root_dir = farm_dir.join("root");
    farm::private_dir(&root_dir, None);
    let ca_file = root_dir.join("ca.pem");
    farm::put(&ca_file, &ca_pem, 0o644, None);
    let coordinator_cert = pki.coordinator(&root_dir);
    let hosts_dir = farm_dir.join("hosts");
    farm::private_dir(&hosts_dir, None);
    let hosts = NODES
        .iter()
        .map(|node| Host::new(node, pki.storage_host(&hosts_dir, node), "1.0.0"))
        .collect();

    // The coordinator: the stand-in issuer, an allow-list that lets the token set tierstore,
    // and a services file of kbf's entry and tierstore's, read by the owner alone.
    let issuer = Issuer::start();
    let control_url = format!(
        "https://localhost:{}",
        farm::CONTROL.rsplit(':').next().unwrap()
    );
    let coordinator_dir = farm_dir.join("coordinator");
    fs::create_dir_all(&coordinator_dir).unwrap();
    let oidc = coordinator_dir.join("oidc.toml");
    farm::put(
        &oidc,
        &farm::allow_list_services(&issuer.url, &control_url, &["kbf", SERVICE]),
        0o644,
        None,
    );
    let trust = coordinator_dir.join("issuer.pem");
    farm::put(&trust, &issuer.cert_pem, 0o644, None);
    let services = coordinator_dir.join("services.toml");
    farm::put(
        &services,
        &format!("{KBF_ENTRY}\n{TIERSTORE_ENTRY}"),
        0o600,
        None,
    );
    let coordinator = farm::start_coordinator_with(
        &coordinator_binary,
        &coordinator_dir,
        &coordinator_cert,
        &ca_file,
        &oidc,
        &trust,
        &[format!("--services-file={}", services.display())],
    );
    let mut farm = Farm {
        coordinator,
        fleet: Fleet { hosts },
        ca_file,
        ca_pem,
        control_url,
        trust,
        issuer,
        xtask,
    };

    // The hosts beat until the coordinator's quiet start is over and it has heard of all six.
    let until = Instant::now() + PATIENCE;
    loop {
        farm.beat(None);
        let status = farm.status();
        let heard = status["per_service"][SERVICE]["hosts"]
            .as_array()
            .map_or(0, Vec::len);
        if status["quiet"] == false && heard == NODES.len() {
            break;
        }
        assert!(
            Instant::now() < until,
            "the coordinator did not hear from the six hosts: {status}"
        );
        sleep(Duration::from_secs(1));
    }
    let kbf_before = farm.status()["desired"].clone();

    // The farm is at 1.0.0, as the last release's deploy left it: converged at once.
    let (code, said) = farm.deploy("deploy_farm_1", "1.0.0", None);
    assert!(code.success(), "{said}");

    // Gamma for 2.0.0: the coordinator rolls it host by host and says when it is converged.
    let (code, said) = farm.deploy("deploy_farm_2", "2.0.0", None);
    assert!(code.success(), "{said}");
    assert_eq!(farm.fleet.running(), ["2.0.0"; 6]);
    let status = farm.status();
    assert_eq!(status["services"][SERVICE]["version"], "2.0.0", "{status}");
    let convergence = &status["per_service"][SERVICE]["convergence"];
    assert_eq!(convergence["state"], "converged", "{status}");
    assert_eq!(convergence["desired"], "2.0.0", "{status}");
    assert_eq!(status["desired"], kbf_before, "kbf's desired state moved");

    // A version no host can install: the deploy fails, and no host moved.
    let (code, said) = farm.deploy("deploy_farm_refused", "3.0.0", Some("3.0.0"));
    assert!(!code.success(), "{said}");
    assert_eq!(farm.fleet.running(), ["2.0.0"; 6]);
    assert_eq!(
        farm.status()["desired"],
        kbf_before,
        "kbf's desired state moved"
    );
}
