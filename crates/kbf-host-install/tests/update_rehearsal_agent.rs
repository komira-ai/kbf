//! The update rehearsal of the control loop: the host of the first rehearsal
//! (`update_rehearsal.rs`), laid out by hand from release N, is updated to release N+1 by the
//! real agent, which the real coordinator orders, which `cargo xtask deploy-farm` told
//! "desired = N+1" with the version text `cargo xtask release farm-version` prints, as the
//! release workflow's gamma job does.
//!
//! What runs, all on the hosted runner under the real systemd:
//!
//! - the coordinator of this checkout (`kbf-coordinator`, built by the job) with a throwaway
//!   farm CA, a certificate of its own from that CA, an operator token, and an allow-list for the
//!   release workflow's OIDC token as `docs/deploy/kbf-updater.md` writes it (the `workflow_run`
//!   event, the `release` environment, `main`). Its issuer is a stand-in on loopback
//!   (`rehearsal/issuer.rs`) that serves the discovery document and the keys the coordinator
//!   fetches and the token service `deploy-farm` asks, so the coordinator's verifier and
//!   `deploy-farm`'s client run unchanged; only the issuer is not GitHub's;
//! - `cargo xtask deploy-farm` (this checkout's), given `--version` from `release farm-version`
//!   (this checkout's), which downloads release N+1's version facts: the form is the one gamma
//!   passes, so a form the hosts never report fails here;
//! - release N's `kbf-updater agent` (its own binary, from release N's tarball) as the packaged
//!   unit `packaging/host-agent/linux/kbf-updater-agent.service` runs it, as `kbf-updater`, with
//!   its certificate placed (`--client-cert`, `--client-key`, `--ca`; `--cordon-dir` is the unit's
//!   state directory, as the packaged unit's comment says for placed certificates), the
//!   simplest of the supported ways: enrolling needs a token and the frontend's enroll listener for a result that
//!   is the same files;
//! - release N's helper and `kbf` on the host, as in the first rehearsal;
//! - a real frontend of release N (`kbf --mode=frontend`, a SQLite state store, the farm CA, the
//!   operator API and the enroll listener) that the host's storage node registers with, as the
//!   storage member whose state and build the agent reads from the frontend's worker listener
//!   (`GET /v1/nodes/{id}/storage-health`) with its certificate alone, no token. The host was
//!   provisioned by hand, so the operator adopts it (`POST /v1/enroll/nodes/{id}:adopt`), the
//!   registry row that joins the node to its member.
//!
//! What proves what: the coordinator only knows the host by its heartbeats, so the host
//! appearing in its status at release N proves the agent dialled it over mutual TLS with the
//! certificate the farm CA issued; the host at N+1 with `last_update` `succeeded` proves the
//! coordinator ordered it and the agent ran the A/B (fetch and verify, drain, switch,
//! health, confirm, undrain) through the helper; `deploy-farm` ending 0 proves the coordinator
//! reported the farm converged on the version `farm-version` printed.
//!
//! What is not covered: the host is a storage node without a daemon half, so the frontend does
//! not know it as a worker and the drain and the undrain are answered "no such node", which the
//! agent takes as nothing to drain; a drain of a node with leases is not rehearsed (a daemon
//! half needs a container runtime). The minimum of storage hosts that must stay up is 0 (the
//! farm has the one host; with the default 1 the coordinator never orders the last storage
//! host). The issuer is a stand-in for GitHub's, and the coordinator's address is loopback.
//!
//! It needs Linux, systemd, root and the network, so it is `#[ignore]`d and runs through
//! `cargo xtask sudo-test -p kbf-host-install --test update_rehearsal_agent` on a GitHub-hosted
//! runner, after the job built `kbf-coordinator`. The setup it shares with the first rehearsal
//! is in `rehearsal/mod.rs`; the farm is in `rehearsal/farm.rs`.

#![cfg(target_os = "linux")]

mod rehearsal;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use rehearsal::farm::{self, PATIENCE};
use rehearsal::issuer::{Issuer, REQUEST_TOKEN};
use rehearsal::{
    CHANNEL, HELPER_UNIT, KBF_UNIT, RELEASES, UPDATER, answers, install_file, journal, run,
    systemctl, until,
};

/// The unit of the agent, as the packaged one is named.
const AGENT_UNIT: &str = "kbf-updater-agent.service";
/// The account `kbf` runs as in its package's unit.
const SERVICE_ACCOUNT: &str = "kbf-server";
/// Where the agent's program goes, beside the helper's.
const AGENT_BINARY: &str = "/usr/libexec/kbf/kbf-updater";
/// The program of release N that is the frontend.
const KBF: &str = "kbf";

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

/// The packaged agent unit with its command line replaced by `exec`.
fn agent_unit(host_agent: &Path, exec: &str) -> String {
    let packaged = fs::read_to_string(host_agent.join(AGENT_UNIT)).unwrap();
    let lines: Vec<&str> = packaged.lines().collect();
    let at: Vec<usize> = (0..lines.len())
        .filter(|i| lines[*i].starts_with("ExecStart="))
        .collect();
    assert_eq!(
        at.len(),
        1,
        "one ExecStart in the packaged unit: {packaged}"
    );
    let mut out: Vec<String> = lines.iter().map(|l| (*l).to_owned()).collect();
    out[at[0]] = format!("ExecStart={exec}");
    out.join("\n") + "\n"
}

/// The agent's `StateDirectory` in the packaged unit: the one place it may write under
/// `ProtectSystem=strict`, where it keeps the marks of its cordons.
const AGENT_STATE_DIR: &str = "/var/lib/kbf-updater";

/// The command line of the agent the rehearsal runs, with its certificate placed in
/// `updater_files` (read-only for the unit) and its cordon marks in the unit's state directory.
fn agent_exec(updater_files: &Path) -> String {
    let path = |p: &Path| p.display().to_string();
    format!(
        "{AGENT_BINARY} agent --coordinator localhost:{} --frontend localhost:{} \
         --client-cert {} --client-key {} --ca {} --cordon-dir {AGENT_STATE_DIR} \
         --channel {CHANNEL} --heartbeat-secs 5 --health-poll-secs 2",
        farm::HEARTBEAT.rsplit(':').next().unwrap(),
        farm::FRONTEND_WORKER.rsplit(':').next().unwrap(),
        path(&updater_files.join("updater.pem")),
        path(&updater_files.join("updater.key")),
        path(&updater_files.join("ca.pem")),
    )
}

/// The storage members of the frontend, once every one is `active` on `commit`.
fn members_active_on(commit: &str) -> bool {
    let members = farm::members();
    let list = members["members"].as_array().cloned().unwrap_or_default();
    !list.is_empty()
        && list
            .iter()
            .all(|m| m["state"] == "active" && m["build_commit"] == commit)
}

/// Waits until `ready` holds, as long as [`PATIENCE`] allows, and stops early when one of the
/// farm's processes has ended.
fn wait(what: &str, procs: &mut [&mut farm::Proc], mut ready: impl FnMut() -> bool) {
    let until = std::time::Instant::now() + PATIENCE;
    while !ready() {
        for proc in procs.iter_mut() {
            proc.alive();
        }
        assert!(
            std::time::Instant::now() < until,
            "timed out waiting for {what}"
        );
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

#[test]
#[ignore = "needs Linux, systemd, root and the network: cargo xtask sudo-test"]
fn the_agent_updates_the_host_when_the_coordinator_is_told_the_next_release() {
    let r = rehearsal::plan();
    let (old, new) = (&r.from.commit, &r.to.commit);
    let coordinator_binary = built("kbf-coordinator");
    let xtask = built("xtask");
    let ip = farm::primary_address();
    let storage = format!("{ip}:{}", farm::STORAGE_PORT);

    let mut journal_guard = rehearsal::Journal::of(&[HELPER_UNIT, KBF_UNIT, AGENT_UNIT]);
    rehearsal::fetch_old_release(&r);
    let updater_tar = format!("kbf-updater-{old}-linux-x86_64.tar.gz");
    rehearsal::download(
        &r.token,
        &r.from.tag,
        &r.from.dir,
        std::slice::from_ref(&updater_tar),
    );
    let updater_dir = r.from.dir.join("updater");
    fs::create_dir_all(&updater_dir).unwrap();
    run(
        "untar_updater",
        Command::new("tar")
            .arg("-xzf")
            .arg(r.from.dir.join(&updater_tar))
            .arg("-C")
            .arg(&updater_dir),
    );

    // The farm CA, and the certificates it issues, each in a directory of the account that
    // reads it (a key must be its reader's, and private).
    let farm_dir = r.work.join("farm");
    fs::create_dir_all(&farm_dir).unwrap();
    fs::set_permissions(&farm_dir, fs::Permissions::from_mode(0o755)).unwrap();
    let ca_dir = farm_dir.join("ca");
    let pki = farm::Pki::new(&ca_dir);
    let ca_pem = pki.ca_pem();
    let root_dir = farm_dir.join("root");
    farm::private_dir(&root_dir, None);
    let ca_file = root_dir.join("ca.pem");
    farm::put(&ca_file, &ca_pem, 0o644, None);
    let frontend_cert = pki.frontend(&root_dir);
    let coordinator_cert = pki.coordinator(&root_dir);

    let storage_dir = farm_dir.join("storage");
    let updater_files = farm_dir.join("updater");

    // The host: release N's kbf as a storage node that registers with the frontend. Its
    // accounts (the service's and the updater's) exist once the host is laid out, and the keys
    // are made for them then.
    let path = |p: &Path| p.display().to_string();
    let flags = format!(
        "--storage-listen={storage} --storage-tls-cert={} --storage-tls-key={} \
         --storage-client-ca={} --frontend=https://localhost:{} --advertise-addr={storage}",
        path(&storage_dir.join("storage.pem")),
        path(&storage_dir.join("storage.key")),
        path(&storage_dir.join("ca.pem")),
        farm::FRONTEND_WORKER.rsplit(':').next().unwrap(),
    );
    let unit = rehearsal::kbf_unit(&format!("User={SERVICE_ACCOUNT}"), &flags);
    let host_agent = rehearsal::lay_out(&r, &unit);
    farm::private_dir(&storage_dir, Some(SERVICE_ACCOUNT));
    pki.storage(&storage_dir, &ip, SERVICE_ACCOUNT);
    farm::put(
        &storage_dir.join("ca.pem"),
        &ca_pem,
        0o644,
        Some(SERVICE_ACCOUNT),
    );
    farm::private_dir(&updater_files, Some(UPDATER));
    pki.updater(&updater_files, UPDATER);
    farm::put(&updater_files.join("ca.pem"), &ca_pem, 0o644, Some(UPDATER));
    run(
        "agent_binary",
        Command::new("install")
            .args(["-D", "-o", "root", "-g", "root", "-m", "0755"])
            .arg(updater_dir.join("kbf-updater"))
            .arg(AGENT_BINARY),
    );
    let agent_exec = agent_exec(&updater_files);
    install_file(
        &agent_unit(&host_agent, &agent_exec),
        &format!("{}/{AGENT_UNIT}", rehearsal::UNIT_DIR),
        0o644,
    );
    systemctl("daemon_reload", &["daemon-reload"]);
    rehearsal::start_helper();

    // The frontend of release N, then the host's storage node, which registers with it; the
    // operator adopts the node so the registry knows what the node advertises.
    let frontend_dir = farm_dir.join("frontend");
    fs::create_dir_all(&frontend_dir).unwrap();
    let mut frontend = farm::start_frontend(
        &r.from.dir.join("release").join(KBF),
        &frontend_dir,
        &ca_dir,
        &frontend_cert,
        &ca_file,
        &storage,
    );
    journal_guard.logs.push(frontend.log.clone());
    farm::adopt(&storage);
    systemctl("start_kbf", &["start", KBF_UNIT]);
    until(KBF_UNIT, || answers(&storage));
    wait(
        "the storage member active on release N",
        &mut [&mut frontend],
        || members_active_on(old),
    );

    // The coordinator, trusting the stand-in issuer.
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
        &farm::allow_list(&issuer.url, &control_url),
        0o644,
        None,
    );
    let trust = coordinator_dir.join("issuer.pem");
    farm::put(&trust, &issuer.cert_pem, 0o644, None);
    let mut coordinator = farm::start_coordinator(
        &coordinator_binary,
        &coordinator_dir,
        &coordinator_cert,
        &ca_file,
        &oidc,
        &trust,
    );
    journal_guard.logs.push(coordinator.log.clone());

    // The agent heartbeats: the coordinator hears from the host at release N.
    systemctl("start_agent", &["start", AGENT_UNIT]);
    wait(
        "the host's heartbeat at release N",
        &mut [&mut frontend, &mut coordinator],
        || {
            farm::host_status(&ca_file).is_some_and(|h| {
                h["version"]
                    .as_str()
                    .is_some_and(|v| v.ends_with(&format!("+g{}", &old[..12])))
            })
        },
    );
    let beat = farm::host_status(&ca_file).unwrap();
    assert_eq!(beat["role"], "storage", "{beat}");
    assert!(beat["updating"].is_null(), "{beat}");
    assert!(beat["last_update"].is_null(), "{beat}");

    // A coordinator that has just started orders nothing and reports no host as live until its
    // quiet start is over; the farm gamma tells has been running for long.
    wait(
        "the coordinator's quiet start to end",
        &mut [&mut frontend, &mut coordinator],
        || farm::status(&ca_file)["quiet"] == false,
    );

    // The version text the farm is told of a release, from the command gamma uses, and gamma's
    // call itself (`--min-up-storage 0`: the rehearsal's farm has one storage host, which the
    // coordinator never orders at the default of 1).
    let version_of = |name: &str, tag: &str, commit: &str| -> String {
        let said = run(
            name,
            Command::new(&xtask)
                .args([
                    "release",
                    "farm-version",
                    "--repo",
                    rehearsal::REPO,
                    "--tag",
                    tag,
                    "--commit",
                    commit,
                ])
                .env("GH_TOKEN", &r.token),
        );
        said.lines()
            .find_map(|l| l.strip_prefix("version="))
            .unwrap_or_else(|| panic!("no version= line: {said}"))
            .to_owned()
    };
    let deploy = |name: &str, version: &str| -> String {
        run(
            name,
            Command::new(&xtask)
                .args(["deploy-farm", "--url", &control_url, "--ca-pem", &ca_pem])
                .args(["--version", version])
                .args(["--min-up-storage", "0", "--min-up-frontend", "0"])
                .args([
                    "--every-secs",
                    "5",
                    "--stall-min",
                    "15",
                    "--deadline-min",
                    "25",
                ])
                .env("ACTIONS_ID_TOKEN_REQUEST_URL", issuer.token_url())
                .env("ACTIONS_ID_TOKEN_REQUEST_TOKEN", REQUEST_TOKEN)
                .env("SSL_CERT_FILE", &trust),
        )
    };

    // The farm has a desired version before this release, as the previous release's gamma left
    // it: the host already runs release N, so the farm is converged at once, on the version
    // text the host reports.
    let previous = version_of("farm_version_n", &r.from.tag, old);
    let settled = deploy("deploy_farm_n", &previous);
    assert!(
        settled.contains(&format!("ok: every live host runs {previous}")),
        "{settled}"
    );

    // Gamma for this release: tell the coordinator, and wait for the farm to converge.
    let desired = version_of("farm_version", &r.to.tag, new);
    assert_eq!(
        desired, r.version,
        "farm-version and the version facts agree"
    );
    let deployed = deploy("deploy_farm", &desired);
    assert!(
        deployed.contains("baseline: hosts live before the deploy: storage 1"),
        "{deployed}"
    );
    assert!(
        deployed.contains(&format!("ok: every live host runs {desired}")),
        "{deployed}"
    );

    // The host runs N+1, and the coordinator's word for it is the agent's.
    let done = farm::host_status(&ca_file).unwrap();
    assert_eq!(done["version"], desired.as_str(), "{done}");
    assert_eq!(done["last_update"]["outcome"], "succeeded", "{done}");
    assert!(done["updating"].is_null(), "{done}");
    let current = fs::read_link(Path::new(RELEASES).join("current")).unwrap();
    assert_eq!(current, Path::new(new), "the current link");
    let pid = systemctl("kbf_pid", &["show", "-p", "MainPID", "--value", KBF_UNIT]);
    let exe = fs::read_link(format!("/proc/{}/exe", pid.trim())).unwrap();
    assert_eq!(
        exe,
        Path::new(RELEASES).join(new).join(KBF),
        "the unit runs N+1's kbf"
    );
    wait(
        "the storage member active on release N+1",
        &mut [&mut frontend, &mut coordinator],
        || members_active_on(new),
    );
    // The frontend's word to the host's updater, with its certificate alone: the host's storage
    // is a member, active on N+1, and no blob is owed a copy.
    let health = farm::storage_health(
        &updater_files.join("ca.pem"),
        &updater_files.join("updater.pem"),
        &updater_files.join("updater.key"),
    );
    assert_eq!(health["half"], "member", "{health}");
    assert_eq!(health["member"]["state"], "active", "{health}");
    assert_eq!(health["member"]["build_commit"], new.as_str(), "{health}");
    let owed = &health["storage"]["under_replicated_blobs"];
    assert!(owed.is_null() || *owed == 0, "{health}");
    assert_eq!(health["storage"]["members_active"], 1, "{health}");
    assert_eq!(
        rehearsal::restarts(AGENT_UNIT),
        0,
        "the agent ran through the update"
    );
    println!("{}", journal(&[HELPER_UNIT, AGENT_UNIT]));
}

/// The values a unit gives `key`, one per line, split on white space.
fn unit_words(unit: &str, key: &str) -> Vec<String> {
    unit.lines()
        .filter_map(|l| l.strip_prefix(&format!("{key}=")))
        .flat_map(|v| v.split_whitespace().map(str::to_owned))
        .collect()
}

/// The value of `flag` in the words of a command line.
fn flag_value<'a>(words: &'a [String], flag: &str) -> Option<&'a str> {
    let at = words.iter().position(|w| w == flag)?;
    words.get(at + 1).map(String::as_str)
}

/// The agent refuses to start with a cordon directory it cannot write, and the packaged unit
/// runs it under `ProtectSystem=strict`: the directory the rehearsal's command line leaves it
/// (`--cordon-dir`, else `--state-dir`, else the directory of `--client-key`) must be inside
/// the unit's `StateDirectory` or `ReadWritePaths`, or the agent never starts in the rehearsal.
#[test]
fn the_rehearsals_agent_can_write_the_directory_of_its_cordons() {
    let host_agent = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/host-agent/linux");
    let files = Path::new(rehearsal::WORK).join("farm/updater");
    let exec = agent_exec(&files);
    let unit = agent_unit(&host_agent, &exec);
    assert_eq!(unit_words(&unit, "ProtectSystem"), ["strict"]);
    let words: Vec<String> = exec.split_whitespace().map(str::to_owned).collect();
    let cordons = flag_value(&words, "--cordon-dir")
        .or_else(|| flag_value(&words, "--state-dir"))
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(flag_value(&words, "--client-key").expect("--client-key"))
                .parent()
                .unwrap()
                .to_path_buf()
        });
    let mut writable: Vec<PathBuf> = unit_words(&unit, "StateDirectory")
        .iter()
        .map(|d| Path::new("/var/lib").join(d))
        .collect();
    writable.extend(
        unit_words(&unit, "ReadWritePaths")
            .iter()
            .map(|p| PathBuf::from(p.trim_start_matches(['-', '+']))),
    );
    assert!(
        writable.iter().any(|w| cordons.starts_with(w)),
        "{} is not inside the unit's StateDirectory or ReadWritePaths ({writable:?})",
        cordons.display()
    );
}
