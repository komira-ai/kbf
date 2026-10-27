//! A service other than kbf on the Linux backend: its release is fetched from its own
//! repository, checked against its own workflow and laid out in its own tree, and goes through
//! the same install, switch, confirm and revert as kbf's, with the units its descriptor names.

use std::cell::Cell;
use std::fs;
use std::path::Path;
use std::rc::Rc;

use super::{
    A, Borrowed, COMMIT, Kind, Rig, binary, file, named_tarball, refused, spawn_args, tar_gz,
    tarball_name, version,
};
use crate::local::answer::{Answer, Code};
use crate::local::backend::{Backend, Fault};
use crate::local::hub::{Hub, describe, production};
use crate::local::linux::production_for;
use crate::local::request::Addressed;
use crate::local::services::Services;
use crate::request::{Channel, Unit};
use crate::service::{Descriptor, ServiceName};
use crate::trust::Trust;

const JS_REPO: &str = "owner/js";
const JS_WORKFLOW: &str = "owner/js/.github/workflows/release.yml";

fn js_descriptor() -> Descriptor {
    Descriptor {
        name: ServiceName::new("js-agent").unwrap(),
        artifact: "js-agent".to_owned(),
        binary: "js-agent".to_owned(),
        tree: "js-agent".to_owned(),
        units: vec![Unit::owned_by("js-agent", "js-agent.service").unwrap()],
    }
}

/// The channels file of a host that updates kbf (`stable`) and `js-agent` (`js-stable`, from its
/// own repository and workflow, which the file trusts).
fn both_channels() -> String {
    format!(
        "trusted_workflows = [\"{JS_WORKFLOW}\"]\n\
         [services.js-agent]\nartifact = \"js-agent\"\nbinary = \"js-agent\"\ntree = \"js-agent\"\nunits = [\"js-agent.service\"]\n\
         [channels.stable]\nsource = \"github\"\n\
         [channels.js-stable]\nservice = \"js-agent\"\nsource = \"github\"\nrepository = \"{JS_REPO}\"\nsigner_workflow = \"{JS_WORKFLOW}\"\nartifact_prefix = \"js-agent\"\n"
    )
}

fn js_kind() -> Kind {
    Kind {
        descriptor: js_descriptor(),
        channels: both_channels(),
    }
}

fn js_stable() -> Channel {
    Channel::new("js-stable").unwrap()
}

/// The hub of a rig: the descriptors come from its channels file, and the backend of any
/// described service is the rig's.
fn hub<'a>(rig: &'a Rig, now: &Rc<Cell<u64>>) -> Hub<'a> {
    let trust = rig.backend.host.trust.clone();
    let (reader, path) = (trust.clone(), rig.backend.allowlist.clone());
    let clock = Rc::clone(now);
    Hub {
        describe: Box::new(move || describe(&reader, &path)),
        make: Box::new(move |_| Ok(Box::new(Borrowed(&rig.backend)))),
        state_dir: rig.dir.to_path_buf(),
        trust,
        window_secs: 600,
        clock: Rc::new(move || clock.get()),
    }
}

fn ask(hub: &Hub<'_>, line: &str) -> Answer {
    let addressed = Addressed::parse(line).unwrap();
    hub.handle(&addressed.service, &addressed.request)
}

fn units_restarted(rig: &Rig) -> Vec<String> {
    rig.systemctl()
}

/// The release is listed from, downloaded from and attested for the service's own repository
/// and workflow (kbf's pair is not tried), its files carry its prefix, and it is laid out as
/// `releases/<commit>/js-agent` in its own tree, beside the running one, with nothing moved and
/// kbf's tree never made.
#[test]
fn a_service_release_is_fetched_from_its_repository_and_laid_out_in_its_own_tree() {
    let rig = Rig::build("svc-stage", &js_kind());
    assert_eq!(rig.backend.stage(&version(), &js_stable()).unwrap(), COMMIT);

    let release = rig.releases().join(COMMIT);
    assert_eq!(fs::read(release.join("js-agent")).unwrap(), binary("b"));
    assert_eq!(
        fs::read_to_string(release.join("VERIFIED")).unwrap(),
        format!("{COMMIT}\n")
    );
    assert!(!release.join("kbf").exists());
    assert_eq!(rig.link("current").as_deref(), Some(A), "no link moved");
    assert!(rig.systemctl().is_empty(), "nothing restarts");
    assert!(
        !rig.root().join("var/lib/kbf-installer").exists(),
        "kbf's tree is not made"
    );

    let verify = spawn_args(&rig, 0);
    let value = |flag: &str| verify[verify.iter().position(|a| a == flag).unwrap() + 1].clone();
    assert_eq!(value("--repo"), JS_REPO);
    assert_eq!(value("--signer-workflow"), JS_WORKFLOW);
    assert_eq!(value("--source-digest"), COMMIT);
    assert!(verify.contains(&"--deny-self-hosted-runners".to_owned()));
    let attestations = rig
        .installer
        .borrow()
        .iter()
        .filter(|s| s.program.file_name().is_some_and(|n| n == "gh"))
        .count();
    assert_eq!(
        attestations, 1,
        "one signer is tried, not kbf's pair as well"
    );

    let calls = rig.calls.borrow();
    let listed = calls.iter().find(|c| c.program.ends_with("curl")).unwrap();
    assert!(
        listed
            .args
            .last()
            .unwrap()
            .contains("/repos/owner/js/releases"),
        "{listed:?}"
    );
    let download = calls
        .iter()
        .find(|c| c.args.first().map(String::as_str) == Some("release"))
        .unwrap();
    assert!(download.args.contains(&named_tarball("js-agent", COMMIT)));
    assert!(!download.args.contains(&tarball_name(COMMIT)));
    let at = download.args.iter().position(|a| a == "--repo").unwrap();
    assert_eq!(download.args[at + 1], JS_REPO);
}

/// The release contents check is the descriptor's: a tarball that holds kbf's program and not
/// the service's, or both, is refused and nothing is left behind; the service's own program
/// is what is accepted.
#[test]
fn the_program_a_release_must_hold_is_the_services_not_kbfs() {
    for (name, items) in [
        ("kbf-only", vec![file("kbf", &binary("b"))]),
        (
            "both",
            vec![file("js-agent", &binary("b")), file("kbf", &binary("b"))],
        ),
        ("other", vec![file("js-agent-extra", &binary("b"))]),
    ] {
        let rig = Rig::build(&format!("svc-contents-{name}"), &js_kind());
        rig.offer_as("js-agent", COMMIT, &tar_gz(&items));
        let fault = rig.backend.stage(&version(), &js_stable()).unwrap_err();
        assert!(refused(Code::VerifyFailed)(&fault), "{name}: {fault:?}");
        rig.assert_nothing_installed();
    }
}

/// The other way: a kbf release must still hold `kbf`, so a tarball that holds a service's
/// program is no kbf release.
#[test]
fn a_kbf_release_must_still_hold_kbf() {
    let rig = Rig::build("svc-kbf-contents", &Kind::kbf());
    rig.offer(COMMIT, &tar_gz(&[file("js-agent", &binary("b"))]));
    let fault = rig.stage().unwrap_err();
    assert!(refused(Code::VerifyFailed)(&fault), "{fault:?}");
    rig.assert_nothing_installed();
}

/// A service installs, switches and confirms through the hub into its own release layout: the
/// links and the state of its tree move, the units restarted are its descriptor's alone, its
/// A/B state is `state-js-agent.json` beside kbf's name, and `query` tells what runs and what
/// is on trial.
#[test]
fn a_service_installs_switches_and_confirms_in_its_own_layout() {
    let rig = Rig::build("svc-flow", &js_kind());
    let now = Rc::new(Cell::new(1_000_u64));
    let hub = hub(&rig, &now);
    let v = version();
    let v = v.as_str();

    assert!(matches!(
        ask(&hub, &format!("install js-agent={v} from js-stable")),
        Answer::Staged { current: false, ref commit, .. } if commit == COMMIT
    ));
    assert_eq!(
        rig.link("current").as_deref(),
        Some(A),
        "install moves nothing"
    );
    let switched = ask(&hub, &format!("switch js-agent={v}"));
    assert!(matches!(
        switched,
        Answer::Switched {
            deadline_unix: 1_600,
            ..
        }
    ));
    assert_eq!(rig.link("current").as_deref(), Some(COMMIT));
    assert_eq!(rig.link("previous").as_deref(), Some(A));
    let restarted = units_restarted(&rig);
    assert!(!restarted.is_empty());
    assert!(
        restarted.iter().all(|c| c.ends_with("js-agent.service")),
        "{restarted:?}"
    );

    let Answer::Queried(view) = ask(&hub, "query js-agent") else {
        panic!("not a query")
    };
    assert_eq!(view.commit.as_deref(), Some(COMMIT));
    assert_eq!(
        view.pending.as_ref().map(|p| p.commit.as_str()),
        Some(COMMIT)
    );

    assert!(matches!(
        ask(&hub, &format!("confirm js-agent={v}")),
        Answer::Confirmed { .. }
    ));
    assert!(rig.state().pending_confirm.is_none());
    assert_eq!(rig.state().current.unwrap().to_string(), COMMIT);
    assert!(rig.dir.join("state-js-agent.json").exists());
    assert!(
        !rig.dir.join("state.json").exists(),
        "kbf's state is not touched"
    );
    // The repeat of a confirm that was answered and lost is answered again.
    assert!(matches!(
        ask(&hub, &format!("confirm js-agent={v}")),
        Answer::Confirmed { .. }
    ));
}

/// A service's release that is not confirmed by its deadline is put back by the tick, with the
/// service's units restarted; before the deadline the tick does nothing. A service that is
/// confirmed is left alone.
#[test]
fn a_services_unconfirmed_release_is_reverted_at_its_deadline() {
    let rig = Rig::build("svc-tick", &js_kind());
    let now = Rc::new(Cell::new(1_000_u64));
    let hub = hub(&rig, &now);
    let v = version();
    ask(
        &hub,
        &format!("install js-agent={} from js-stable", v.as_str()),
    );
    ask(&hub, &format!("switch js-agent={}", v.as_str()));
    now.set(1_599);
    assert!(hub.tick().is_empty());
    now.set(1_600);
    let done = hub.tick();
    assert_eq!(done.len(), 1);
    assert_eq!(done[0].0.as_str(), "js-agent");
    assert!(
        matches!(done[0].1, Answer::Reverted { .. }),
        "{:?}",
        done[0].1
    );
    assert_eq!(rig.state().current.unwrap().to_string(), A);
    assert_eq!(rig.link("current").as_deref(), Some(A));
    assert!(hub.tick().is_empty(), "nothing is on trial now");

    // A request can revert one that is on trial.
    ask(
        &hub,
        &format!("install js-agent={} from js-stable", v.as_str()),
    );
    ask(&hub, &format!("switch js-agent={}", v.as_str()));
    assert!(matches!(
        ask(&hub, "revert js-agent"),
        Answer::Reverted { .. }
    ));
    assert_eq!(rig.state().current.unwrap().to_string(), A);
}

/// A service the configuration does not describe is refused with its own code, whatever the
/// verb, and nothing runs; a service's request that names kbf's channel, or the channel of
/// another service, is not allowed.
#[test]
fn a_request_for_an_unconfigured_service_or_another_services_channel_is_refused() {
    let rig = Rig::build("svc-refused", &js_kind());
    let now = Rc::new(Cell::new(1_000_u64));
    let hub = hub(&rig, &now);
    let v = version();
    let v = v.as_str();
    for line in [
        "query js-server".to_owned(),
        "revert js-server".to_owned(),
        format!("install js-server={v} from js-stable"),
        format!("switch js-server={v}"),
        format!("confirm js-server={v}"),
    ] {
        assert!(
            matches!(
                ask(&hub, &line),
                Answer::Refused(Code::ServiceNotConfigured, _)
            ),
            "{line}"
        );
    }
    // js-agent from kbf's channel: not allowed, and nothing was fetched.
    assert!(matches!(
        ask(&hub, &format!("install js-agent={v} from stable")),
        Answer::Refused(Code::ChannelNotAllowed, _)
    ));
    assert!(
        rig.calls.borrow().is_empty(),
        "nothing was fetched or restarted"
    );
}

/// kbf's backend does not take a service's channel, with the same file that describes both:
/// the refusal is `channel_not_allowed` and nothing is fetched. With the same file its own
/// channel is still fine.
#[test]
fn kbf_does_not_use_a_services_channel() {
    let rig = Rig::build(
        "svc-kbf-js-channel",
        &Kind {
            channels: both_channels(),
            ..Kind::kbf()
        },
    );
    let fault = rig.backend.stage(&version(), &js_stable()).unwrap_err();
    assert!(refused(Code::ChannelNotAllowed)(&fault), "{fault:?}");
    assert!(rig.calls.borrow().is_empty());
    assert_eq!(rig.stage().unwrap(), COMMIT);
}

/// A configuration that cannot be trusted or read is the answer to a service's request, as it
/// is to kbf's (`config_unsafe`, `config_invalid`), and hides no trial for the tick to find
/// until the file is back.
#[test]
fn a_configuration_that_cannot_be_read_refuses_the_service_and_the_tick_waits() {
    use std::os::unix::fs::PermissionsExt;
    let rig = Rig::build("svc-config", &js_kind());
    let now = Rc::new(Cell::new(1_000_u64));
    let hub = hub(&rig, &now);
    let v = version();
    ask(
        &hub,
        &format!("install js-agent={} from js-stable", v.as_str()),
    );
    ask(&hub, &format!("switch js-agent={}", v.as_str()));
    now.set(2_000);

    let file = rig.backend.allowlist.clone();
    let good = fs::read_to_string(&file).unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o666)).unwrap();
    assert!(matches!(
        ask(&hub, "query js-agent"),
        Answer::Refused(Code::ConfigUnsafe, _)
    ));
    assert!(hub.tick().is_empty());
    fs::write(&file, "this is [not toml").unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(matches!(
        ask(&hub, "query js-agent"),
        Answer::Refused(Code::ConfigInvalid, _)
    ));
    assert!(hub.tick().is_empty());
    fs::remove_file(&file).unwrap();
    assert!(matches!(
        ask(&hub, "query js-agent"),
        Answer::Refused(Code::ConfigInvalid, _)
    ));

    fs::write(&file, good).unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
    let done = hub.tick();
    assert!(matches!(done[0].1, Answer::Reverted { .. }), "{done:?}");
}

/// A backend that cannot be made for a described service is the answer to its request; the
/// tick skips it.
#[test]
fn a_backend_that_cannot_be_made_is_the_answer() {
    let rig = Rig::build("svc-make", &js_kind());
    let now = Rc::new(Cell::new(1_000_u64));
    let mut broken = hub(&rig, &now);
    broken.make = Box::new(|_| Err(Fault::refused(Code::ConfigInvalid, "no backend")));
    assert!(matches!(
        ask(&broken, "query js-agent"),
        Answer::Refused(Code::ConfigInvalid, _)
    ));
    assert!(broken.tick().is_empty());
}

/// What the program starts with for a service: the descriptor's tree, program and units in
/// the layout of the installer, a scratch tag and an install lock of the service's own, and
/// the channels file of the local socket; a descriptor whose tree is not a tree is refused.
#[test]
fn the_production_host_of_a_service_is_laid_out_by_its_descriptor() {
    let host = production_for(&js_descriptor()).unwrap();
    assert_eq!(
        host.layout.join(&host.layout.releases()),
        Path::new("/var/lib/js-agent/releases")
    );
    assert_eq!(host.subject.binary, "js-agent");
    assert_eq!(host.subject.name.as_str(), "js-agent");
    assert_eq!(host.units.len(), 1);
    assert_eq!(host.scratch_tag.as_deref(), Some("js-agent"));
    assert_eq!(
        host.lock_file(),
        Path::new("/run/kbf-host-install-js-agent.lock")
    );
    assert_eq!(
        host.allowlist,
        Path::new("/etc/kbf/host-install-releases.toml")
    );
    let bad = Descriptor {
        tree: "../x".to_owned(),
        ..js_descriptor()
    };
    assert!(
        production_for(&bad)
            .err()
            .unwrap()
            .contains("not usable in a layout")
    );
}

/// The hub of the program: it makes the backend of a described service by that layout, refuses
/// a descriptor that cannot be laid out with a code, keeps its state beside the helper's and
/// reads the real clock.
#[test]
fn the_production_hub_makes_backends_by_descriptor() {
    let hub = production(&Trust::root(), Path::new("/var/lib/kbf-host-install"), 900);
    assert_eq!(hub.state_dir, Path::new("/var/lib/kbf-host-install"));
    assert_eq!(hub.window_secs, 900);
    assert!((hub.clock)() > 1_700_000_000);
    let backend = (hub.make)(&js_descriptor()).unwrap();
    assert_eq!(
        backend.lock_file(),
        Path::new("/run/kbf-host-install-js-agent.lock")
    );
    let bad = Descriptor {
        tree: "../x".to_owned(),
        ..js_descriptor()
    };
    let fault = (hub.make)(&bad).err().unwrap();
    assert!(refused(Code::ConfigInvalid)(&fault), "{fault:?}");
}
