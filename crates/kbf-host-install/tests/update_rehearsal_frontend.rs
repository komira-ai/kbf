//! The update rehearsal of a frontend container: a frontend of release N runs as a container
//! under the packaged `kbf-host-local-frontend.service`, the helper that serves a host whose
//! frontend is a container, configured by `/etc/kbf/host-install-frontend.toml`; the helper is
//! asked, through its local socket as the updater's agent asks it, to `install`, `switch` to and
//! `confirm` release N+1, whose attested frontend image it fetches from this repository's GitHub
//! releases itself.
//!
//! N+1 is the release named in `/var/tmp/kbf-update-rehearsal.to`, or the newest published
//! release when the file is absent; N is the published release before it. What runs, all on the
//! hosted runner under the real systemd and the runner's Docker:
//!
//! - release N's `kbf-host-install` at `/usr/libexec/kbf`, with the trust files and accounts of
//!   N's package, as in the first rehearsal (`update_rehearsal.rs`; the setup is in
//!   `rehearsal/mod.rs`), and the helper's unit of this checkout, unchanged: the unit is the
//!   packaged `kbf-host-local-frontend.service`;
//! - a storage node, release N's `kbf` as a unit on a `file://` cold store, which the frontend
//!   reaches over loopback;
//! - release N's frontend image, as the release publishes it (`kbf-frontend-image-<commit>-...`,
//!   checked against the release's `SHA256SUMS`), loaded by Docker as `kbf-frontend:<commit>`,
//!   and a container made from it with the options of the configuration file
//!   (`rehearsal/frontend.rs`): the host network, a SQLite state store, the image's own user.
//!
//! The helper is asked directly over its socket, as the first rehearsal does, and not through
//! the agent: the agent and the coordinator that orders it are rehearsed by
//! `update_rehearsal_agent.rs`, and a frontend host's agent asks the helper the same five
//! requests; the whole control loop here would add a farm CA, a coordinator and a second
//! frontend for the agent's heartbeat and none of what the helper does to the container.
//!
//! What it proves, in order:
//!
//! 1. a switch that cannot drain the frontend (the file names an operator token the container
//!    does not have) fails, the helper recreates release N's container from release N's image
//!    with the same configuration, and it is running, ready and N's, with nothing pending;
//! 2. the switch with the right file: while the helper replaces the container, the operator API
//!    of N's container is seen standing down (`/readyz` 503, `draining`), and Docker's record
//!    shows the drain run in the container before the container ended and the next one started;
//! 3. afterwards the container runs release N+1's binary (asked of the binary inside it), its
//!    `/readyz` is 200 and its `/healthz` names N+1's commit, it was made with the restart policy
//!    and user of the file, and `confirm` leaves nothing pending.
//!
//! Not covered: a second frontend. The file lists no `peers`, so the helper looks at no other
//! frontend before it takes this one down (it refuses with `peer_not_ready` when a listed peer
//! does not answer `/readyz` with 200; that guard is unit-tested in the helper). The helper needs
//! none to run. The coordinator's order, the agent, the frontend's drain with open Execute
//! streams, a bridge network with published ports, and the image's attestation of release N (the
//! helper checks N+1's: offline, with the trust files of N's package) are not exercised here.
//!
//! It needs Linux, systemd, Docker, root and the network, so it is `#[ignore]`d and runs through
//! `cargo xtask sudo-test -p kbf-host-install --test update_rehearsal_frontend` on a
//! GitHub-hosted runner: in the `update-rehearsal` workflow, and in the release before the
//! gamma stage.

#![cfg(target_os = "linux")]

mod rehearsal;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use rehearsal::frontend::{self, Events, Logs, Setup, Watch};
use rehearsal::talk::ask;
use rehearsal::{
    CHANNEL, FRONTEND_CONFIG, FRONTEND_HELPER_UNIT, KBF_UNIT, UNIT_DIR, answers, install_file,
    systemctl,
};

/// A token path the container does not have.
const NO_SUCH_TOKEN: &str = "/etc/kbf/no-such-token";

/// Requires the container to be release `commit`'s, running, and ready, as the helper's own
/// check is: the binary inside it names the commit, and the operator API says the same.
fn serves(name: &str, commit: &str) {
    let state = frontend::inspect(&format!("{name}_inspect"));
    assert_eq!(state["State"]["Running"], true, "{name}: {state}");
    assert_eq!(
        frontend::running_commit(&format!("{name}_binary")),
        commit,
        "{name}: the binary in the container"
    );
    assert_eq!(frontend::ready(), Some(200), "{name}: /readyz");
    assert_eq!(
        frontend::health_commit().as_deref(),
        Some(&commit[..12]),
        "{name}: /healthz"
    );
}

/// Requires the host's records to name `commit` and no trial.
fn settled_on(probe: &Path, name: &str, commit: &str) {
    let answer = ask(probe, name, "query");
    assert_eq!(answer["result"], "query", "{answer}");
    assert_eq!(answer["commit"], commit, "{answer}");
    assert!(answer["pending"].is_null(), "no trial is pending: {answer}");
}

#[test]
#[ignore = "needs Linux, systemd, Docker, root and the network: cargo xtask sudo-test"]
fn the_helper_updates_a_frontend_container_from_the_last_release_to_the_next() {
    let r = rehearsal::plan();
    let (old, new, version) = (&r.from.commit, &r.to.commit, &r.version);
    rehearsal::fetch_old_release(&r);
    let image = format!("kbf-frontend-image-{old}-linux-x86_64.oci.tar");
    rehearsal::download(
        &r.token,
        &r.from.tag,
        &r.from.dir,
        std::slice::from_ref(&image),
    );
    let _journal = rehearsal::Journal::of(&[FRONTEND_HELPER_UNIT, KBF_UNIT]);
    let _logs = Logs;

    // The host: N's helper and trust files, the storage node, and the packaged frontend unit.
    let host_agent = rehearsal::lay_out_host(&r);
    rehearsal::install_kbf_unit(&rehearsal::kbf_unit(
        "DynamicUser=yes",
        &format!("--storage-listen={}", frontend::STORAGE),
    ));
    let unit = fs::read_to_string(host_agent.join(FRONTEND_HELPER_UNIT)).unwrap();
    assert!(unit.contains(" --frontend-container"), "{unit}");
    install_file(&unit, &format!("{UNIT_DIR}/{FRONTEND_HELPER_UNIT}"), 0o644);
    systemctl("daemon_reload", &["daemon-reload"]);
    systemctl("start_kbf", &["start", KBF_UNIT]);
    rehearsal::until(KBF_UNIT, || answers(frontend::STORAGE));

    // Release N's frontend: its image, loaded as an operator loads it, and a container made
    // with the options of the file the helper is given.
    let loaded = frontend::docker(
        "image_load",
        &[
            "image",
            "load",
            "--input",
            &r.from.dir.join(&image).display().to_string(),
        ],
    );
    let tag = format!("kbf-frontend:{old}");
    assert!(loaded.contains(&tag), "docker loaded: {loaded}");
    let setup = Setup::prepare(&r.work);
    install_file(
        &setup.config(frontend::TOKEN_IN_CONTAINER),
        FRONTEND_CONFIG,
        0o644,
    );
    let args = setup.docker_run(&tag);
    frontend::docker(
        "frontend_run",
        &args.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    frontend::wait("release N's frontend is ready", || {
        frontend::ready() == Some(200)
    });
    serves("n_serves", old);

    rehearsal::start_helper_unit(FRONTEND_HELPER_UNIT);
    let probe = r.work.join("probe");
    fs::copy(std::env::current_exe().unwrap(), &probe).unwrap();
    fs::set_permissions(&probe, fs::Permissions::from_mode(0o755)).unwrap();
    settled_on(&probe, "query_before", old);

    // Release N+1's frontend image, fetched, checked and loaded by the helper.
    let install = format!("install kbf={version} from {CHANNEL}");
    let staged = ask(&probe, "install", &install);
    rehearsal::require_staged(&staged, &[FRONTEND_HELPER_UNIT, KBF_UNIT]);
    assert_eq!(staged["commit"], new.as_str(), "{staged}");
    let new_image = frontend::docker(
        "new_image",
        &[
            "image",
            "inspect",
            "--format",
            "{{.Id}}",
            &format!("kbf-frontend:{new}"),
        ],
    );
    assert!(!new_image.trim().is_empty(), "N+1's image is loaded");

    // 1. A switch that cannot drain the frontend: the helper puts release N's container back.
    let before = frontend::inspect("before_failed_switch")["Id"].clone();
    install_file(&setup.config(NO_SUCH_TOKEN), FRONTEND_CONFIG, 0o644);
    let failed = ask(
        &probe,
        "switch_without_a_token",
        &format!("switch kbf={version}"),
    );
    assert_eq!(failed["result"], "failed", "{failed}");
    assert_eq!(failed["reason"], "switch_failed", "{failed}");
    let back = frontend::inspect("after_failed_switch");
    assert_ne!(
        back["Id"], before,
        "the helper made the container again from release N's image"
    );
    assert_eq!(
        back["HostConfig"]["RestartPolicy"]["Name"],
        "unless-stopped"
    );
    serves("n_back", old);
    settled_on(&probe, "query_after_failed_switch", old);

    // 2. The switch with the right file. The helper has to stage again: a failed switch keeps
    // no claim on the image.
    install_file(
        &setup.config(frontend::TOKEN_IN_CONTAINER),
        FRONTEND_CONFIG,
        0o644,
    );
    let staged = ask(&probe, "install_again", &install);
    rehearsal::require_staged(&staged, &[FRONTEND_HELPER_UNIT, KBF_UNIT]);
    let (watch, events) = (Watch::start(), Events::start());
    let switched = ask(&probe, "switch", &format!("switch kbf={version}"));
    let (seen, events) = (watch.finish(), events.finish());
    assert_eq!(switched["result"], "switched", "{switched}");
    assert_eq!(switched["commit"], new.as_str(), "{switched}");

    // N's container stood down before the switch: its operator API said `draining` and
    // `/readyz` 503, and only afterwards did N+1's commit appear.
    let (old12, new12) = (&old[..12], &new[..12]);
    let first_new = seen
        .iter()
        .position(|s| s.commit.as_deref() == Some(new12))
        .unwrap_or_else(|| panic!("N+1's frontend never answered: {seen:?}"));
    let of_n = || {
        seen[..first_new]
            .iter()
            .filter(|s| s.commit.as_deref() == Some(old12))
    };
    assert!(
        of_n().any(|s| s.draining == Some(true)),
        "release N's frontend was not seen draining: {seen:?}"
    );
    assert!(
        of_n().any(|s| s.ready == Some(503)),
        "release N's frontend was not seen answering /readyz 503: {seen:?}"
    );
    // Docker's record: the drain ran in the container, then the container ended, then the
    // next one was started.
    let at = |from: usize, pick: &dyn Fn(&str) -> bool| {
        events
            .iter()
            .enumerate()
            .skip(from)
            .find(|(_, l)| pick(l))
            .map(|(i, _)| i)
    };
    let drain = at(0, &|l| {
        l.starts_with("exec_start") && l.contains("cluster drain")
    })
    .unwrap_or_else(|| panic!("no drain was run in the container: {events:?}"));
    let died = at(drain, &|l| l.starts_with("die "))
        .unwrap_or_else(|| panic!("the container did not end after the drain: {events:?}"));
    at(died, &|l| l.starts_with("start "))
        .unwrap_or_else(|| panic!("no container was started after the old one ended: {events:?}"));

    // 3. The container is N+1's, as the file says to make it.
    serves("n_plus_1_serves", new);
    let state = frontend::inspect("n_plus_1");
    assert_eq!(
        state["HostConfig"]["RestartPolicy"]["Name"],
        "unless-stopped"
    );
    assert_eq!(state["HostConfig"]["NetworkMode"], "host");
    assert_eq!(state["Config"]["User"], "65532:65532");
    assert_eq!(
        state["Image"].as_str().map(str::trim),
        Some(new_image.trim()),
        "the container is made from the image the helper loaded"
    );
    let confirmed = ask(&probe, "confirm", &format!("confirm kbf={version}"));
    assert_eq!(confirmed["result"], "confirmed", "{confirmed}");
    settled_on(&probe, "query_after", new);
    serves("n_plus_1_confirmed", new);
}
