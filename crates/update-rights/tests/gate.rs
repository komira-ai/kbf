//! The decision on a new declaration: install it, or defer until it is approved.

mod support;

use std::collections::BTreeSet;

use support::{sample, with};
use update_rights::names::Capability;
use update_rights::{
    Approvals, Declaration, Deferral, Digest, FIRST_INSTALL, Gate, RIGHTS_CHANGE, gate,
};

fn digest(seed: &str) -> Digest {
    Digest::of(seed.as_bytes())
}

fn approved(digests: &[&Digest]) -> Approvals {
    let text: String = digests.iter().map(|d| format!("{d}\n")).collect();
    Approvals::parse(&text).unwrap()
}

fn defer(widened: &[&str], digest: &Digest) -> Gate {
    Gate::Deferred(Deferral {
        reason: RIGHTS_CHANGE,
        digest: digest.clone(),
        widened: widened.iter().map(|w| (*w).to_owned()).collect(),
    })
}

/// Catches a first install going through unasked: with no active declaration there is nothing
/// to be narrower than, so it needs an approval, whoever the hook runs as.
#[test]
fn a_first_install_is_deferred_until_approved() {
    let d = digest("new");
    let incoming = sample();
    assert_eq!(
        gate(None, &incoming, &d, &Approvals::default()),
        defer(&[FIRST_INSTALL], &d)
    );
    assert_eq!(gate(None, &incoming, &d, &approved(&[&d])), Gate::Install);
    // An approval of another declaration does not carry over.
    assert_eq!(
        gate(None, &incoming, &d, &approved(&[&digest("other")])),
        defer(&[FIRST_INSTALL], &d)
    );
}

/// Catches the kbf and js-agent hooks, which run as root, installing without the one-time
/// approval the design says they need.
#[test]
fn a_root_hook_is_deferred_as_a_rights_change_the_first_time() {
    let root = Declaration::parse(&with("user = \"tierstore-update\"", "user = \"root\"")).unwrap();
    let d = digest("root declaration");
    let verdict = gate(None, &root, &d, &Approvals::default());
    assert_eq!(verdict, defer(&[FIRST_INSTALL], &d));
    assert_eq!(RIGHTS_CHANGE, "rights_change");
    assert_eq!(gate(None, &root, &d, &approved(&[&d])), Gate::Install);
}

/// Catches an equal or narrower declaration being asked about, and a wider one being installed.
#[test]
fn equal_and_narrower_install_and_wider_is_deferred_with_the_fields() {
    let active = sample();
    let d = digest("incoming");
    let none = Approvals::default();
    assert_eq!(gate(Some(&active), &active, &d, &none), Gate::Install);
    let mut narrower = active.clone();
    narrower.hook.read_paths.clear();
    narrower.hook.timeout_secs.cleanup -= 1;
    assert_eq!(gate(Some(&active), &narrower, &d, &none), Gate::Install);
    let mut wider = active.clone();
    wider.hook.write_paths.insert("/srv/more".into());
    wider.hook.step_capabilities.verify = Some(BTreeSet::from([Capability::try_from(
        "CAP_NET_ADMIN".to_owned(),
    )
    .unwrap()]));
    assert_eq!(
        gate(Some(&active), &wider, &d, &none),
        defer(&["hook.write_paths", "hook.step_capabilities.verify"], &d)
    );
}

/// Catches an approval that is ignored once the declaration is wider, or kept for a different
/// declaration: it is the exact declaration (its digest) that is approved.
#[test]
fn a_wider_declaration_installs_once_that_exact_declaration_is_approved() {
    let active = sample();
    let mut wider = active.clone();
    wider.hook.user = "root".into();
    let d = digest("wider");
    assert!(matches!(
        gate(Some(&active), &wider, &d, &Approvals::default()),
        Gate::Deferred(_)
    ));
    assert_eq!(
        gate(Some(&active), &wider, &d, &approved(&[&d])),
        Gate::Install
    );
    assert!(matches!(
        gate(Some(&active), &wider, &d, &approved(&[&digest("another")])),
        Gate::Deferred(_)
    ));
}

/// Catches an approved digest being asked about again even when the declaration is narrower than
/// the active one: the approval wins, and the equal case does not need it.
#[test]
fn an_approved_digest_installs_whatever_it_is_compared_with() {
    let active = sample();
    let d = digest("same");
    assert_eq!(
        gate(Some(&active), &active, &d, &approved(&[&d])),
        Gate::Install
    );
}
