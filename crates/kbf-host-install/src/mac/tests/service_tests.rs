//! The channels file with services: a channel of a service other than kbf names its origin
//! and its service is described, and a request for a service takes only that service's
//! channels.

use super::fx::{Fx, write};
use crate::mac::channel::{ChannelError, MacAllowlist};
use crate::mac::release::Origin;
use crate::outcome::Refusal;
use crate::request::Channel;
use crate::service::ServiceName;

const REPO: &str = "owner/js";
const WORKFLOW: &str = "owner/js/.github/workflows/release.yml";
const TRUSTED: &str = "trusted_workflows = [\"owner/js/.github/workflows/release.yml\"]\n";
const DESCRIPTOR: &str = "[services.js-agent]\nartifact = \"js-agent\"\nbinary = \"js-agent\"\ntree = \"js-agent\"\nunits = [\"js-agent.service\"]\n";

fn load(fx: &Fx, text: &str) -> Result<MacAllowlist, Refusal> {
    write(&fx.host.allowlist, 0o644, text.as_bytes());
    MacAllowlist::load(&fx.host.trust, &fx.host.allowlist)
}

fn js_channel(keys: &[&str]) -> String {
    let mut text =
        String::from("[channels.js-stable]\nservice = \"js-agent\"\nsource = \"github\"\n");
    for key in keys {
        text.push_str(&match *key {
            "repository" => format!("repository = \"{REPO}\"\n"),
            "signer_workflow" => format!("signer_workflow = \"{WORKFLOW}\"\n"),
            "artifact_prefix" => "artifact_prefix = \"js-agent\"\n".to_owned(),
            other => panic!("{other}"),
        });
    }
    text
}

fn js_file(keys: &[&str]) -> String {
    format!("{TRUSTED}{DESCRIPTOR}{}", js_channel(keys))
}

const ALL: [&str; 3] = ["repository", "signer_workflow", "artifact_prefix"];

fn channel(name: &str) -> Channel {
    Channel::new(name).unwrap()
}

fn js() -> ServiceName {
    ServiceName::new("js-agent").unwrap()
}

/// A channel of a service other than kbf names its repository, signer workflow and artifact
/// prefix, all three: whichever is left out, the file is invalid as a whole, because nothing
/// is defaulted to kbf's for a service that is not kbf. All three make a usable channel.
#[test]
fn a_channel_of_another_service_must_name_all_three_origin_keys() {
    let fx = Fx::new("mac-svc-keys");
    let mut subsets = 0;
    for mask in 0..7_u8 {
        let keys: Vec<&str> = ALL
            .iter()
            .enumerate()
            .filter(|(i, _)| mask & (1 << i) != 0)
            .map(|(_, k)| *k)
            .collect();
        assert_eq!(
            load(&fx, &js_file(&keys)).err(),
            Some(Refusal::ConfigInvalid),
            "{keys:?}"
        );
        subsets += 1;
    }
    assert_eq!(subsets, 7, "every proper subset of the three keys");
    let list = load(&fx, &js_file(&ALL)).unwrap();
    let found = list
        .channel_of(&fx.host.trust, &channel("js-stable"), &js())
        .unwrap();
    assert_eq!(
        found.origin,
        Origin::new(REPO, WORKFLOW, "js-agent").unwrap()
    );
}

/// The service of a channel must be described, and its descriptor must agree with the channel:
/// the artifact is the channel's prefix. A bad name, a missing descriptor, a different artifact
/// and a channel whose files are kbf's directory or sums are each invalid.
#[test]
fn a_channel_of_another_service_needs_a_descriptor_that_agrees_with_it() {
    let fx = Fx::new("mac-svc-descriptor");
    let channel_text = js_channel(&ALL);
    let invalid = |text: String, why: &str| {
        assert_eq!(
            load(&fx, &text).err(),
            Some(Refusal::ConfigInvalid),
            "{why}"
        );
    };
    invalid(format!("{TRUSTED}{channel_text}"), "no descriptor");
    invalid(
        format!(
            "{TRUSTED}{}{channel_text}",
            DESCRIPTOR.replace("artifact = \"js-agent\"", "artifact = \"other\"")
        ),
        "the artifact is not the channel's prefix",
    );
    invalid(
        format!(
            "{TRUSTED}{DESCRIPTOR}{}",
            channel_text.replace("service = \"js-agent\"", "service = \"Js\"")
        ),
        "a service name that is not one",
    );
    invalid(
        format!(
            "{TRUSTED}{DESCRIPTOR}{}",
            channel_text.replace("service = \"js-agent\"", "service = \"other\"")
        ),
        "another service's descriptor does not describe this one",
    );
    invalid(
        format!(
            "{TRUSTED}{DESCRIPTOR}{}sums = \"/etc/js.sums\"\n",
            channel_text
        ),
        "sums name kbf's files",
    );
    // A descriptor that does not parse or is not its own spoils the file, used or not.
    for descriptor in [
        DESCRIPTOR.replace(
            "units = [\"js-agent.service\"]",
            "units = [\"sshd.service\"]",
        ),
        DESCRIPTOR.replace("tree = \"js-agent\"", "tree = \"kbf-installer\""),
        DESCRIPTOR.replace("[services.js-agent]", "[services.kbf]"),
        format!("{DESCRIPTOR}unknown = 1\n"),
    ] {
        invalid(
            format!("{TRUSTED}{descriptor}{}", js_channel(&[])),
            &descriptor,
        );
        invalid(
            format!("{TRUSTED}{descriptor}[channels.stable]\nsource = \"github\"\n"),
            "unused",
        );
    }
}

/// A channel that says `service = "kbf"` is kbf's channel as one that says nothing is, with
/// kbf's defaults.
#[test]
fn a_channel_may_say_that_it_is_kbfs() {
    let fx = Fx::new("mac-svc-explicit-kbf");
    let list = load(
        &fx,
        "[channels.stable]\nservice = \"kbf\"\nsource = \"github\"\n",
    )
    .unwrap();
    let found = list.channel(&fx.host.trust, &channel("stable")).unwrap();
    assert_eq!(found.origin, Origin::kbf());
}

/// The signer of a channel of another service must be listed in `trusted_workflows`, and it
/// is not enough that it is kbf's own: kbf's workflow does not sign another service's
/// releases.
#[test]
fn the_signer_of_another_service_must_be_listed() {
    let fx = Fx::new("mac-svc-trusted");
    let unlisted = format!("{DESCRIPTOR}{}", js_channel(&ALL));
    let list = load(&fx, &unlisted).unwrap();
    let error = list
        .channel_of(&fx.host.trust, &channel("js-stable"), &js())
        .unwrap_err();
    assert_eq!(error.refusal(), Refusal::ChannelNotAllowed);
    assert!(
        matches!(error, ChannelError::UntrustedSigner { .. }),
        "{error:?}"
    );
    assert!(error.to_string().contains(WORKFLOW));

    // Another workflow listed does not help.
    let other = "trusted_workflows = [\"owner/js/.github/workflows/other.yml\"]\n";
    let list = load(&fx, &format!("{other}{unlisted}")).unwrap();
    assert!(
        list.channel_of(&fx.host.trust, &channel("js-stable"), &js())
            .is_err()
    );

    // Kbf's own signer, named by a channel of another service, still has to be listed.
    let kbfs = js_channel(&["artifact_prefix"])
        + "repository = \"komira-ai/kbf\"\nsigner_workflow = \"komira-ai/kbf/.github/workflows/artifacts.yml\"\n";
    let list = load(&fx, &format!("{DESCRIPTOR}{kbfs}")).unwrap();
    let error = list
        .channel_of(&fx.host.trust, &channel("js-stable"), &js())
        .unwrap_err();
    assert!(
        matches!(error, ChannelError::UntrustedSigner { .. }),
        "{error:?}"
    );
    let listed = "trusted_workflows = [\"komira-ai/kbf/.github/workflows/artifacts.yml\"]\n";
    let list = load(&fx, &format!("{listed}{DESCRIPTOR}{kbfs}")).unwrap();
    assert!(
        list.channel_of(&fx.host.trust, &channel("js-stable"), &js())
            .is_ok()
    );
}

/// A request for a service takes only the channels of that service: kbf cannot use a
/// service's channel, a service cannot use kbf's, and one service cannot use another's.
#[test]
fn a_service_uses_only_its_own_channels() {
    let fx = Fx::new("mac-svc-own");
    let second = DESCRIPTOR.replace("js-agent", "js-server");
    let text = format!(
        "{TRUSTED}{DESCRIPTOR}{second}[channels.stable]\nsource = \"github\"\n{}",
        js_channel(&ALL)
    );
    let list = load(&fx, &text).unwrap();
    let not_allowed = |channel_name: &str, service: &ServiceName| {
        list.channel_of(&fx.host.trust, &channel(channel_name), service)
            .map(drop)
            .map_err(|e| e.refusal())
    };
    assert_eq!(not_allowed("js-stable", &js()), Ok(()));
    assert_eq!(not_allowed("stable", &ServiceName::kbf()), Ok(()));
    assert_eq!(
        not_allowed("js-stable", &ServiceName::kbf()),
        Err(Refusal::ChannelNotAllowed)
    );
    assert_eq!(
        not_allowed("stable", &js()),
        Err(Refusal::ChannelNotAllowed)
    );
    assert_eq!(
        not_allowed("js-stable", &ServiceName::new("js-server").unwrap()),
        Err(Refusal::ChannelNotAllowed)
    );
    assert_eq!(
        not_allowed("missing", &js()),
        Err(Refusal::ChannelNotAllowed)
    );
    // The call every kbf caller makes is kbf's.
    assert_eq!(
        list.channel(&fx.host.trust, &channel("js-stable"))
            .map_err(|e| e.refusal())
            .map(drop),
        Err(Refusal::ChannelNotAllowed)
    );
    let names: Vec<&str> = list.services().map(|d| d.name.as_str()).collect();
    assert_eq!(names, ["js-agent", "js-server"]);
}
