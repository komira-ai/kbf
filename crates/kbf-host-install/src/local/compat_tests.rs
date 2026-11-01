//! The helper and the release before it read each other's data: the state file each writes,
//! the request lines, and the `query` line the agent reads. The previous release's shapes are
//! fixtures here, copied from it (the format rule of CONTRIBUTING.md: a release starts on the
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
