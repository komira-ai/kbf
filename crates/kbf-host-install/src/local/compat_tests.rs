//! The helper and the release before it read each other's data: the state file each writes,
//! the request lines, and the `query` line the agent reads. The previous release's shapes are
//! fixtures here, copied from it (rule B8 of AGENTS.md: a release starts on the
//! data the other wrote).

use std::collections::BTreeMap;
use std::fs;

use serde::Deserialize;

use super::{Fx, VA, VB};
use crate::local::answer::Answer;
use crate::local::request::LocalRequest;
use crate::request::Version;

/// The previous release's `state.json`, read as strictly as it reads it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct PreviousSaved {
    #[serde(default)]
    versions: BTreeMap<String, String>,
    #[serde(default)]
    staged: Option<PreviousStaged>,
    #[serde(default)]
    last: Option<PreviousLast>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct PreviousStaged {
    version: String,
    commit: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreviousLast {
    target: String,
    ended: String,
    why: Option<String>,
}

/// Catches a state file the previous release cannot read once this one has written it: after an
/// update confirmed with a last good (the prune, the listing and `retain.json` all in play), the
/// state holds only what the previous release reads.
#[test]
fn the_state_this_helper_writes_is_read_by_the_previous_one() {
    let fx = Fx::new("compat-state-out");
    fx.world.borrow_mut().retain_file = Some(fx.dir.join("retain.json"));
    fx.install(VB);
    fx.ask(&format!("switch kbf={VB}"));
    let answer = fx.ask(&format!("confirm kbf={VB} last_good={VA}"));
    assert_eq!(answer, Answer::Confirmed { version: VB.into() });
    let text = fs::read_to_string(fx.dir.join("state.json")).unwrap();
    let previous: PreviousSaved =
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}: {text}"));
    let last = previous.last.expect("the update is told");
    assert_eq!(
        (last.target.as_str(), last.ended.as_str()),
        (VB, "succeeded")
    );
    assert_eq!(last.why, None);
}

/// Catches this helper not reading the state file the previous release wrote: its version
/// text is what the query tells.
#[test]
fn the_state_the_previous_helper_wrote_is_read() {
    let fx = Fx::new("compat-state-in");
    let written = format!(
        "{{\"versions\":{{\"{commit}\":\"{VA}\"}},\"staged\":null,\
         \"last\":{{\"target\":\"{VA}\",\"ended\":\"succeeded\",\"why\":null}}}}",
        commit = super::A
    );
    fs::write(fx.dir.join("state.json"), written).unwrap();
    fs::set_permissions(
        fx.dir.join("state.json"),
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )
    .unwrap();
    let view = fx.query();
    assert_eq!(view.version.as_deref(), Some(VA));
    assert_eq!(view.last.map(|l| l.outcome), Some("succeeded"));
}

/// Catches a request line of the previous agent meaning something else here: each of the five
/// lines it sends parses as it did, and a confirm without a last good carries none.
#[test]
fn the_previous_agents_requests_mean_what_they_did() {
    for line in [
        "query".to_owned(),
        "revert".to_owned(),
        format!("install kbf={VA} from stable"),
        format!("switch kbf={VA}"),
    ] {
        assert!(LocalRequest::parse(&line).is_ok(), "{line}");
    }
    let confirm = LocalRequest::parse(&format!("confirm kbf={VA}")).unwrap();
    assert!(matches!(
        confirm,
        LocalRequest::Confirm { version, last_good: None } if version.as_str() == VA
    ));
}

/// Catches a confirm with a last good that the previous helper would run: its grammar takes the
/// rest of the line as the version, which a space makes no version, so it answers `bad_command`
/// and runs nothing. (An agent sends the part only to a helper of this release or later.)
#[test]
fn the_previous_helper_refuses_a_confirm_with_a_last_good_and_runs_nothing() {
    let line = format!("confirm kbf={VB} last_good={VA}");
    let rest = line.strip_prefix("confirm kbf=").unwrap();
    assert!(Version::new(rest).is_err(), "{rest}");
}

/// The previous release's `retain.json`, read as strictly as this test can: every key it knew.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)] // Fields read only by the parse that the test asserts succeeds.
struct PreviousRetained {
    #[serde(default)]
    pinned: Vec<String>,
    #[serde(default)]
    last_good: Option<String>,
    #[serde(default)]
    versions: BTreeMap<String, String>,
}

fn kept(commit: &str) -> kbf_retain::Kept {
    kbf_retain::Kept {
        commit: kbf_retain::CommitId::new(commit).unwrap(),
        version: None,
        why: Vec::new(),
        version_left_out: false,
    }
}

/// Catches an activate that leaves data the previous helper cannot read. A helper of the
/// previous release starts on a host this one activated with a pin and a forced lock:
/// - `state.json` holds only what it reads strictly, `last` in its words;
/// - `retain.json` holds only the keys it knew, the pin in `pinned`;
/// - the lock file holding a process id is read by its lock readers (which look at the `flock`,
///   never the content) as free when free and held when held;
/// - `lock-takes.jsonl` and the hold file are names it never opens.
#[test]
fn what_an_activate_writes_is_read_by_the_previous_helper() {
    let fx = Fx::new("compat-activate-out");
    let retain = fx.dir.join("retain.json");
    {
        let mut world = fx.world.borrow_mut();
        world.retain_file = Some(retain.clone());
        world.kept = vec![kept(super::B), kept(super::A)];
        world.on_disk.insert(super::B.to_owned());
    }
    fs::create_dir_all(fx.dir.join("agent")).unwrap();
    let ab = Fx::process(&fx.dir, &fx.world, &fx.clock).holding(fx.dir.join("agent/pin.json"));
    let lock = fx.dir.join("lock");
    let _dead = {
        let file = crate::host::open_lock(&lock).unwrap();
        file.try_lock().unwrap();
        file
    };
    let how = crate::local::core::activate::How {
        pin: true,
        force_lock: true,
    };
    let answer = ab.activate(
        &crate::local::request::Target::parse(super::B).unwrap(),
        how,
    );
    assert!(
        matches!(
            answer,
            Answer::Activated {
                pinned: true,
                lock_taken: true,
                ..
            }
        ),
        "{answer:?}"
    );

    let text = fs::read_to_string(fx.dir.join("state.json")).unwrap();
    let previous: PreviousSaved =
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}: {text}"));
    let last = previous.last.expect("the activate is told");
    assert_eq!(
        (last.target.as_str(), last.ended.as_str()),
        (super::B, "succeeded")
    );
    let text = fs::read_to_string(&retain).unwrap();
    let previous: PreviousRetained =
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}: {text}"));
    assert_eq!(previous.pinned, [super::B]);
    assert_eq!(
        fs::read_to_string(&lock).unwrap(),
        format!("{}\n", std::process::id()),
        "the taker's id"
    );
    assert!(!crate::locks::flock_held(&lock), "free, and read as free");
    let held = crate::host::take_lock(&lock).unwrap();
    assert!(crate::locks::flock_held(&lock), "held, and read as held");
    drop(held);
    assert!(
        fx.dir
            .join(crate::local::core::activate::LOCK_TAKES)
            .exists()
    );
}

/// Catches this helper not activating on a host the previous helper left: its `state.json` (with
/// a release staged) and its empty lock file are read, the version text it recorded names the
/// release, and the staged release that is activated is no longer staged.
#[test]
fn an_activate_runs_on_what_the_previous_helper_wrote() {
    let fx = Fx::new("compat-activate-in");
    {
        let mut world = fx.world.borrow_mut();
        world.kept = vec![kept(super::B), kept(super::A)];
        world.on_disk.insert(super::B.to_owned());
    }
    let written = format!(
        "{{\"versions\":{{\"{b}\":\"{VB}\"}},\"staged\":{{\"version\":\"{VB}\",\"commit\":\"{b}\"}},\
         \"last\":{{\"target\":\"{VA}\",\"ended\":\"succeeded\",\"why\":null}}}}",
        b = super::B
    );
    fs::write(fx.dir.join("state.json"), written).unwrap();
    fs::set_permissions(
        fx.dir.join("state.json"),
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )
    .unwrap();
    fs::write(fx.dir.join("lock"), "").unwrap();
    let answer = fx.ask(&format!("activate kbf={VB}"));
    assert!(
        matches!(&answer, Answer::Activated { version: Some(v), .. } if v == VB),
        "{answer:?}"
    );
    let text = fs::read_to_string(fx.dir.join("state.json")).unwrap();
    let previous: PreviousSaved = serde_json::from_str(&text).unwrap();
    assert!(previous.staged.is_none());
}

/// Catches an `activate` line that the previous helper would run as something else: it starts
/// with none of the forms that helper reads (`query`, `revert`, `install kbf=`, `switch kbf=`,
/// `confirm kbf=`), so it answers `bad_command` and runs nothing. (An agent sends it only to a
/// helper of this release or later.)
#[test]
fn the_previous_helper_refuses_an_activate_and_runs_nothing() {
    let line = format!("activate kbf={VB}");
    for form in [
        "query",
        "revert",
        "install kbf=",
        "switch kbf=",
        "confirm kbf=",
    ] {
        assert!(!line.starts_with(form), "{form}");
    }
}
