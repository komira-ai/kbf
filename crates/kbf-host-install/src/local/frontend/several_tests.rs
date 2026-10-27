//! Several frontend containers on one host: `group_add` on every container the helper makes,
//! and one helper per container, each with its own configuration file, record, A/B state and
//! lock.

use std::fs;
use std::os::unix::fs::MetadataExt;

use super::fake::{Source, World};
use super::tests::{CONFIG, NEW, OLD, Rig, VERSION, failed, put, refused, spec};
use super::{Files, Frontend};
use crate::host::Host;
use crate::local::answer::Answer;
use crate::local::core::Ab;
use crate::local::request::LocalRequest;
use crate::trust::Trust;

const NEW_ID: &str = "sha256:new";
const OLD_ID: &str = "sha256:old";

fn with_groups(groups: &str) -> String {
    CONFIG.replace(
        "user = \"65532:65532\"",
        &format!("user = \"65532:65532\"\ngroup_add = {groups}"),
    )
}

fn words(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| (*a).to_owned()).collect()
}

/// The container made for a switch and the one put back by a revert both carry the gids of
/// the file the switch began with, as `--group-add` words between `--user` and `--publish`,
/// whatever the file says by then.
#[test]
fn group_add_gids_reach_the_new_container_and_the_one_put_back() {
    let rig = Rig::with_config("groups", &with_groups("[\"990\", \"1001\"]"));
    assert!(matches!(rig.update(), Answer::Switched { .. }));
    let fixed = |image: &str| {
        words(&[
            "run",
            "--detach",
            "--name",
            "kbf-frontend",
            "--restart",
            "unless-stopped",
            "--network",
            "bridge",
            "--user",
            "65532:65532",
            "--group-add",
            "990",
            "--group-add",
            "1001",
            "--publish",
            "127.0.0.1:8982:8982",
            "--publish",
            "8980:8980",
            "--volume",
            "/etc/kbf:/etc/kbf:ro",
            "--volume",
            "/var/lib/kbf:/var/lib/kbf",
            image,
            "--config",
            "/etc/kbf/kbf.toml",
        ])
    };
    assert_eq!(rig.world.runs().last().unwrap(), &fixed(NEW_ID));
    // The file loses its groups meanwhile; the revert is made from the marker.
    put(&rig.config_path(), CONFIG);
    assert!(matches!(rig.ask("revert"), Answer::Reverted { .. }));
    assert_eq!(rig.world.runs().last().unwrap(), &fixed(OLD_ID));
}

/// A switch whose container fails to become ready puts the old one back with the same gids.
#[test]
fn group_add_gids_reach_the_old_container_after_a_failed_switch() {
    let rig = Rig::with_config("groups-fail", &with_groups("[\"990\"]"));
    rig.world
        .0
        .borrow_mut()
        .never_ready
        .insert(NEW_ID.to_owned());
    assert!(failed(crate::local::answer::Code::SwitchFailed)(
        &rig.update()
    ));
    let runs = rig.world.runs();
    assert_eq!(runs.len(), 2);
    for (run, image) in runs.iter().zip([NEW_ID, OLD_ID]) {
        let at = run.iter().position(|w| w == "--group-add").unwrap();
        assert_eq!(&run[at..at + 2], ["--group-add", "990"], "{run:?}");
        assert!(run.contains(&image.to_owned()));
    }
}

/// A switch with a gid that is not one is refused as `config_invalid` and nothing is touched.
#[test]
fn a_bad_gid_refuses_the_switch() {
    let rig = Rig::new("bad-gid");
    assert!(matches!(rig.install(), Answer::Staged { .. }));
    let before = rig.steps().len();
    for bad in ["\"0\"", "\"admin\"", "\"-1\"", "\"1 2\"", "\"\""] {
        put(&rig.config_path(), &with_groups(&format!("[{bad}]")));
        assert!(
            refused(crate::local::answer::Code::ConfigInvalid)(&rig.switch()),
            "{bad}"
        );
    }
    assert_eq!(rig.steps().len(), before);
}

const SECOND: &str = r#"
[frontend]
name = "kbf-fe-b"
restart = "unless-stopped"
network = "host"
user = "65532:65532"
group_add = ["990"]
mounts = ["/srv/kbf/fe/b:/var/lib/kbf-server"]
command = ["--config=/etc/kbf/kbf.toml"]
api = "127.0.0.1:28982"
drain_token_file = "/etc/kbf/operator.token"
"#;

/// A second helper over the same host: its own configuration file, container, record, A/B
/// state and lock, as `--frontend-config` gives it.
fn second(rig: &Rig, world: &World, bound: Option<&str>) -> Ab<Frontend<Source, World, World>> {
    let uid = fs::metadata(&*rig.dir).unwrap().uid();
    let trust = Trust::owned_by(uid, &rig.dir);
    let files = Files::of(bound);
    let mut host = Host::for_test(&rig.dir, trust.clone());
    host.install_lock = rig.dir.join("second.lock");
    let backend = Frontend {
        source: Source::new(world, NEW),
        runtime: world.clone(),
        probe: world.clone(),
        host,
        config_file: rig.dir.join("second.toml"),
        record_file: rig.dir.join(&files.record),
        bound_name: bound.map(str::to_owned),
        pace: world.pace(),
    };
    let clock = std::rc::Rc::clone(&rig.clock);
    Ab::new(
        backend,
        rig.dir.join(&files.state),
        trust,
        600,
        Box::new(move || clock.get()),
    )
}

fn second_world() -> World {
    let world = World::default();
    world.serving("kbf-fe-b", "127.0.0.1:28982");
    let old = world.image(OLD, "old");
    let run = super::config::FrontendConfig::parse(SECOND).unwrap().run;
    world.container(&run, &old, OLD);
    assert_eq!(world.will_load(NEW, "new"), NEW_ID);
    world
}

fn switch_on_trial(ab: &Ab<Frontend<Source, World, World>>) {
    let ask = |line: String| ab.handle(&LocalRequest::parse(&line).unwrap());
    assert!(matches!(
        ask(format!("install kbf={VERSION} from stable")),
        Answer::Staged { .. }
    ));
    assert!(matches!(
        ask(format!("switch kbf={VERSION}")),
        Answer::Switched { .. }
    ));
}

/// With `--frontend-config` at a second file, the helper makes that file's container, keeps
/// its record and state under that container's name, and leaves the default helper's record
/// alone: the default helper does not exist on disk, the second one's does.
#[test]
fn a_helper_with_its_own_file_uses_that_container_and_its_own_record() {
    let rig = Rig::new("own-file");
    put(&rig.dir.join("second.toml"), SECOND);
    let world = second_world();
    let helper = second(&rig, &world, Some("kbf-fe-b"));
    switch_on_trial(&helper);
    let run = world.runs().pop().unwrap();
    assert_eq!(
        &run[..4],
        ["run", "--detach", "--name", "kbf-fe-b"],
        "{run:?}"
    );
    assert!(run.contains(&"--group-add".to_owned()) && run.contains(&"--network".to_owned()));
    assert!(rig.dir.join("frontend-kbf-fe-b.json").exists());
    assert!(rig.dir.join("state-frontend-kbf-fe-b.json").exists());
    assert!(!rig.dir.join("frontend.json").exists());
    assert!(!rig.dir.join("state.json").exists());
    // The first helper sees nothing pending and did nothing to its container.
    assert!(rig.query().pending.is_none());
    assert!(rig.world.events().is_empty());
}

/// Two helpers with a release each on trial: past the deadline, the tick of one puts back its
/// own container and the other's record, state and container are as they were.
#[test]
fn one_helper_cannot_revert_the_container_of_the_other() {
    let rig = Rig::new("no-cross-revert");
    put(&rig.dir.join("second.toml"), SECOND);
    let world = second_world();
    let helper = second(&rig, &world, Some("kbf-fe-b"));
    assert!(matches!(rig.update(), Answer::Switched { .. }));
    switch_on_trial(&helper);
    let theirs = fs::read(rig.dir.join("frontend-kbf-fe-b.json")).unwrap();
    let their_state = fs::read(rig.dir.join("state-frontend-kbf-fe-b.json")).unwrap();
    let their_runs = world.runs().len();
    let their_events = world.events().len();
    // Past the deadline of both; only the first helper ticks.
    rig.clock.set(rig.clock.get() + 700);
    assert!(matches!(rig.ab.tick(), Some(Answer::Reverted { .. })));
    assert_eq!(rig.world.made().unwrap().image_id, OLD_ID);
    assert_eq!(world.made().unwrap().image_id, NEW_ID);
    assert_eq!(world.runs().len(), their_runs);
    assert_eq!(world.events().len(), their_events);
    assert_eq!(
        fs::read(rig.dir.join("frontend-kbf-fe-b.json")).unwrap(),
        theirs
    );
    assert_eq!(
        fs::read(rig.dir.join("state-frontend-kbf-fe-b.json")).unwrap(),
        their_state
    );
    // The other one then goes back by itself.
    assert!(matches!(helper.tick(), Some(Answer::Reverted { .. })));
    assert_eq!(world.made().unwrap().image_id, OLD_ID);
}

/// The two helpers' lock files are different, so a switch in one does not hold up the
/// other's revert.
#[test]
fn the_files_of_two_containers_never_meet() {
    let a = Files::of(Some("kbf-fe-a"));
    let b = Files::of(Some("kbf-fe-b"));
    assert_ne!(a.record, b.record);
    assert_ne!(a.state, b.state);
    assert_ne!(a.lock, b.lock);
    let default = Files::of(None);
    assert_eq!(default.record, "frontend.json");
    assert_eq!(default.state, "state.json");
    assert_eq!(default.lock, None);
    assert!(
        ![&a, &b].iter().any(|f| f.record == default.record
            || f.state == default.state
            || f.lock == default.lock)
    );
}

/// A helper started for one container refuses a file that now names another, rather than
/// write that container's state into the first one's record.
#[test]
fn a_file_that_changes_its_container_is_refused() {
    let rig = Rig::new("renamed");
    put(&rig.dir.join("second.toml"), SECOND);
    let world = second_world();
    let helper = second(&rig, &world, Some("kbf-fe-b"));
    assert!(matches!(
        helper.handle(&LocalRequest::parse(&format!("install kbf={VERSION} from stable")).unwrap()),
        Answer::Staged { .. }
    ));
    let before = world.events().len();
    put(
        &rig.dir.join("second.toml"),
        &SECOND.replace("kbf-fe-b", "kbf-fe-a"),
    );
    let answer = helper.handle(&LocalRequest::parse(&format!("switch kbf={VERSION}")).unwrap());
    assert!(
        refused(crate::local::answer::Code::ConfigInvalid)(&answer),
        "{answer:?}"
    );
    assert_eq!(world.events().len(), before);
}

/// A container the operator made by hand (as the single-host layout describes: host network,
/// no published ports, a signer group) is adopted by a file that describes it: the first
/// switch starts from the image it runs, and the container made has the same words.
#[test]
fn an_existing_container_is_adopted_by_a_file_that_describes_it() {
    let rig = Rig::new("adopt");
    let file = r#"
[frontend]
name = "kbf-fe-a"
restart = "unless-stopped"
network = "host"
user = "65532:65532"
group_add = ["990"]
mounts = [
  "/srv/kbf/fe/etc:/etc/kbf:ro",
  "/srv/kbf/fe/a:/var/lib/kbf-server",
  "/srv/kbf/postgres/run:/run/postgresql",
  "/run/kbf-signer:/run/kbf-signer:ro",
  "/srv/kbf/ca:/etc/kbf-frontend-ca:ro",
]
command = ["--config=/etc/kbf/kbf.toml", "--api-listen=127.0.0.1:18982"]
api = "127.0.0.1:18982"
drain_token_file = "/etc/kbf/operator.token"
peers = ["127.0.0.1:28982"]
"#;
    put(&rig.dir.join("second.toml"), file);
    let world = World::default();
    world.serving("kbf-fe-a", "127.0.0.1:18982");
    world
        .0
        .borrow_mut()
        .peers
        .insert("127.0.0.1:28982".parse().unwrap(), 200);
    let old = world.image(OLD, "old");
    // No record yet: the container was made by hand from `kbf-frontend:<OLD>`.
    world.container(&spec(), &old, OLD);
    world.will_load(NEW, "new");
    let helper = second(&rig, &world, Some("kbf-fe-a"));
    switch_on_trial(&helper);
    let run = world.runs().pop().unwrap();
    assert_eq!(
        run,
        words(&[
            "run",
            "--detach",
            "--name",
            "kbf-fe-a",
            "--restart",
            "unless-stopped",
            "--network",
            "host",
            "--user",
            "65532:65532",
            "--group-add",
            "990",
            "--volume",
            "/srv/kbf/fe/etc:/etc/kbf:ro",
            "--volume",
            "/srv/kbf/fe/a:/var/lib/kbf-server",
            "--volume",
            "/srv/kbf/postgres/run:/run/postgresql",
            "--volume",
            "/run/kbf-signer:/run/kbf-signer:ro",
            "--volume",
            "/srv/kbf/ca:/etc/kbf-frontend-ca:ro",
            NEW_ID,
            "--config=/etc/kbf/kbf.toml",
            "--api-listen=127.0.0.1:18982",
        ])
    );
    let record: serde_json::Value =
        serde_json::from_slice(&fs::read(rig.dir.join("frontend-kbf-fe-a.json")).unwrap()).unwrap();
    assert_eq!(record["pending"]["from"], OLD);
    assert_eq!(record["pending"]["from_image"], OLD_ID);
}

fn with_hardening() -> String {
    CONFIG.replace(
        "user = \"65532:65532\"",
        "user = \"65532:65532\"\nread_only = true\ntmpfs = [\"/tmp:rw,size=256m\"]\ncap_drop = [\"ALL\"]\nsecurity_opt = [\"no-new-privileges\"]\nulimits = [\"nofile=1048576:1048576\"]",
    )
}

/// The container made for a switch and the one put back by a revert both carry the hardening
/// of the file the switch began with, whatever the file says by then.
#[test]
fn hardening_reaches_the_new_container_and_the_one_put_back() {
    let rig = Rig::with_config("hardening", &with_hardening());
    assert!(matches!(rig.update(), Answer::Switched { .. }));
    let fixed = |image: &str| {
        words(&[
            "run",
            "--detach",
            "--name",
            "kbf-frontend",
            "--restart",
            "unless-stopped",
            "--network",
            "bridge",
            "--user",
            "65532:65532",
            "--read-only",
            "--tmpfs",
            "/tmp:rw,size=256m",
            "--cap-drop",
            "ALL",
            "--security-opt",
            "no-new-privileges",
            "--ulimit",
            "nofile=1048576:1048576",
            "--publish",
            "127.0.0.1:8982:8982",
            "--publish",
            "8980:8980",
            "--volume",
            "/etc/kbf:/etc/kbf:ro",
            "--volume",
            "/var/lib/kbf:/var/lib/kbf",
            image,
            "--config",
            "/etc/kbf/kbf.toml",
        ])
    };
    assert_eq!(rig.world.runs().last().unwrap(), &fixed(NEW_ID));
    put(&rig.config_path(), CONFIG);
    assert!(matches!(rig.ask("revert"), Answer::Reverted { .. }));
    assert_eq!(rig.world.runs().last().unwrap(), &fixed(OLD_ID));
}

/// A switch whose container fails to become ready puts the old one back with the hardening.
#[test]
fn hardening_reaches_the_old_container_after_a_failed_switch() {
    let rig = Rig::with_config("hardening-fail", &with_hardening());
    rig.world
        .0
        .borrow_mut()
        .never_ready
        .insert(NEW_ID.to_owned());
    assert!(failed(crate::local::answer::Code::SwitchFailed)(
        &rig.update()
    ));
    let runs = rig.world.runs();
    assert_eq!(runs.len(), 2);
    for run in &runs {
        for flag in [
            "--read-only",
            "--tmpfs",
            "--cap-drop",
            "--security-opt",
            "--ulimit",
        ] {
            assert!(run.contains(&flag.to_owned()), "{flag} in {run:?}");
        }
    }
}

/// A switch with a hardening value that is not accepted is refused as `config_invalid` and
/// nothing is touched.
#[test]
fn a_bad_hardening_value_refuses_the_switch() {
    let rig = Rig::new("bad-hardening");
    assert!(matches!(rig.install(), Answer::Staged { .. }));
    let before = rig.steps().len();
    for bad in [
        "security_opt = [\"seccomp=unconfined\"]",
        "security_opt = [\"label=disable\"]",
        "tmpfs = [\"tmp\"]",
        "cap_drop = [\"all\"]",
        "ulimits = [\"nofile=2:1\"]",
    ] {
        put(
            &rig.config_path(),
            &CONFIG.replace(
                "user = \"65532:65532\"",
                &format!("user = \"65532:65532\"\n{bad}"),
            ),
        );
        assert!(
            refused(crate::local::answer::Code::ConfigInvalid)(&rig.switch()),
            "{bad}"
        );
    }
    assert_eq!(rig.steps().len(), before);
}
