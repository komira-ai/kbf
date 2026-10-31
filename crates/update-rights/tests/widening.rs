//! Widening: any change other than removing an element from a set or lowering a number.
//! One mutation per field of the declaration, each of which must be named, and nothing else.

mod support;

use std::collections::BTreeSet;

use support::with;
use update_rights::names::{Capability, Family};
use update_rights::{Declaration, widening};

fn cap(name: &str) -> Capability {
    Capability::try_from(name.to_owned()).unwrap()
}

fn family(name: &str) -> Family {
    Family::try_from(name.to_owned()).unwrap()
}

/// The sample with one environment variable, so every field has a value to change.
fn base() -> Declaration {
    Declaration::parse(&with("environment = { }", "environment = { K = \"v\" }")).unwrap()
}

type Change = fn(&mut Declaration);

/// A field and a change to it, spelt so a closure becomes a function pointer.
fn w(field: &'static str, change: Change) -> (&'static str, Change) {
    (field, change)
}

/// Every field a file can hold, one change that makes it ask for more, and the name the change
/// is reported under.
fn widenings() -> Vec<(&'static str, Change)> {
    vec![
        w("name", |d| d.name = "renamed".into()),
        w("binary", |d| d.binary = "other".into()),
        w("hook_args", |d| d.hook_args.push("--extra".into())),
        w("signer.repository", |d| {
            d.signer.repository = "evil/repo".into()
        }),
        w("signer.workflow", |d| d.signer.workflow = "evil.yml".into()),
        w("uid", |d| d.uid = "someone".into()),
        w("registration_ttl_secs", |d| d.registration_ttl_secs += 1),
        w("health_ports", |d| {
            d.health_ports.insert(9999);
        }),
        w("verify_deadline_secs", |d| d.verify_deadline_secs += 1),
        w("promotion_ceiling_secs", |d| {
            d.promotion_ceiling_secs = Some(3601)
        }),
        w("cleanup_retries", |d| d.cleanup_retries += 1),
        w("state_dir", |d| {
            d.state_dir = "/var/lib/updater/hooks/elsewhere".into()
        }),
        w("boot", |d| d.boot = Some("other".into())),
        w("min_version", |d| d.min_version = Some("0.0.1".into())),
        w("hook.user", |d| d.hook.user = "root".into()),
        w("hook.group", |d| d.hook.group = "root".into()),
        w("hook.capabilities", |d| {
            d.hook.capabilities.insert(cap("CAP_KILL"));
        }),
        w("hook.step_capabilities.prepare", |d| {
            d.hook
                .step_capabilities
                .prepare
                .as_mut()
                .unwrap()
                .insert(cap("CAP_KILL"));
        }),
        w("hook.step_capabilities.cutover", |d| {
            d.hook
                .step_capabilities
                .cutover
                .as_mut()
                .unwrap()
                .insert(cap("CAP_KILL"));
        }),
        w("hook.step_capabilities.verify", |d| {
            d.hook
                .step_capabilities
                .verify
                .as_mut()
                .unwrap()
                .insert(cap("CAP_NET_ADMIN"));
        }),
        w("hook.step_capabilities.rollback", |d| {
            d.hook
                .step_capabilities
                .rollback
                .as_mut()
                .unwrap()
                .insert(cap("CAP_KILL"));
        }),
        w("hook.step_capabilities.cleanup", |d| {
            d.hook
                .step_capabilities
                .cleanup
                .as_mut()
                .unwrap()
                .insert(cap("CAP_KILL"));
        }),
        w("hook.read_paths", |d| {
            d.hook.read_paths.insert("/srv/more".into());
        }),
        w("hook.write_paths", |d| {
            d.hook.write_paths.insert("/srv/more".into());
        }),
        w("hook.units", |d| {
            d.hook.units.insert("tierstore@9303.service".into());
        }),
        w("hook.address_families", |d| {
            d.hook.address_families.insert(family("AF_PACKET"));
        }),
        w("hook.environment.K", |d| {
            d.hook.environment.insert("K".into(), "changed".into());
        }),
        w("hook.timeout_secs.prepare", |d| {
            d.hook.timeout_secs.prepare += 1
        }),
        w("hook.timeout_secs.cutover", |d| {
            d.hook.timeout_secs.cutover += 1
        }),
        w("hook.timeout_secs.verify", |d| {
            d.hook.timeout_secs.verify += 1
        }),
        w("hook.timeout_secs.rollback", |d| {
            d.hook.timeout_secs.rollback += 1
        }),
        w("hook.timeout_secs.cleanup", |d| {
            d.hook.timeout_secs.cleanup += 1
        }),
        w("hook.params.promotion_bound_secs", |d| {
            d.hook
                .params
                .insert("promotion_bound_secs".into(), 901.into());
        }),
    ]
}

/// The names of the leaves of a value, as the file spells them.
fn leaves(prefix: &str, value: &toml::Value, into: &mut BTreeSet<String>) {
    match value.as_table() {
        Some(table) => {
            for (key, inner) in table {
                let name = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                leaves(&name, inner, into);
            }
        }
        None => {
            into.insert(prefix.to_owned());
        }
    }
}

/// Catches a field of the declaration that has no mutation here: a field added to the type and
/// not compared (and not tested) would go through unapproved. The fields are those of the type,
/// read from its serialisation with every optional key set.
#[test]
fn every_field_of_a_declaration_has_a_mutation() {
    let value = toml::Value::try_from(base()).unwrap();
    let mut fields = BTreeSet::new();
    leaves("", &value, &mut fields);
    let tested: BTreeSet<String> = widenings()
        .iter()
        .map(|(name, _)| (*name).to_owned())
        .collect();
    assert_eq!(fields, tested);
}

/// Catches a change that is not reported as widening, or that is reported as more than it is:
/// each change names its own field and no other.
#[test]
fn each_field_that_asks_for_more_is_named() {
    for (field, change) in widenings() {
        let mut changed = base();
        change(&mut changed);
        assert_ne!(changed, base(), "{field}: the change changed nothing");
        assert_eq!(widening(&base(), &changed), [field], "{field}");
    }
}

/// Catches a comparison that only looks one way: the same change made the other way round is a
/// removal or a lowered number for the sets and numbers, and a change again for the rest.
#[test]
fn the_reverse_of_a_widening_is_narrowing_for_sets_and_numbers_only() {
    let always = [
        "name",
        "binary",
        "hook_args",
        "signer.repository",
        "signer.workflow",
        "uid",
        "state_dir",
        "boot",
        "min_version",
        "hook.user",
        "hook.group",
        "hook.environment.K",
    ];
    for (field, change) in widenings() {
        let mut wider = base();
        change(&mut wider);
        let back = widening(&wider, &base());
        if always.contains(&field) {
            assert_eq!(back, [field], "{field}");
        } else {
            // A set that loses an element, a number that falls: narrower.
            assert!(back.is_empty(), "{field}: {back:?}");
        }
    }
}

/// Catches `boot` treated as a plain text: a boot entry point that appears or changes asks for
/// more; one that goes away does not. A first release that is lowered is still a change.
#[test]
fn a_boot_entry_point_that_goes_away_narrows() {
    let mut without = base();
    without.boot = None;
    assert!(widening(&base(), &without).is_empty());
    assert_eq!(widening(&without, &base()), ["boot"]);
    let mut no_min = base();
    no_min.min_version = None;
    assert_eq!(widening(&base(), &no_min), ["min_version"]);
}

/// Catches a missing ceiling being read as a low one: no ceiling is the highest.
#[test]
fn removing_the_promotion_ceiling_widens_and_adding_one_narrows() {
    let mut none = base();
    none.promotion_ceiling_secs = None;
    assert_eq!(widening(&base(), &none), ["promotion_ceiling_secs"]);
    assert!(widening(&none, &base()).is_empty());
    assert!(widening(&none, &none).is_empty());
    let mut lower = base();
    lower.promotion_ceiling_secs = Some(10);
    assert!(widening(&base(), &lower).is_empty());
}

/// Catches an equal declaration being read as a change, and a removal as a widening.
#[test]
fn equal_and_narrower_declarations_widen_nothing() {
    assert!(widening(&base(), &base()).is_empty());
    let mut narrower = base();
    narrower.health_ports.remove(&9301);
    narrower.hook.read_paths.remove("/usr/sbin/nft");
    narrower.hook.units.remove("tierstore@9302.service");
    narrower.hook.address_families.remove(&family("AF_NETLINK"));
    narrower.hook.step_capabilities.prepare = Some(BTreeSet::new());
    narrower
        .hook
        .step_capabilities
        .cutover
        .as_mut()
        .unwrap()
        .clear();
    narrower.hook.environment.clear();
    narrower.hook.params.clear();
    narrower.registration_ttl_secs -= 1;
    narrower.verify_deadline_secs -= 1;
    narrower.promotion_ceiling_secs = Some(1);
    narrower.cleanup_retries = 0;
    narrower.hook.timeout_secs.prepare -= 1;
    narrower.hook.timeout_secs.cutover -= 1;
    narrower.hook.timeout_secs.verify -= 1;
    narrower.hook.timeout_secs.rollback -= 1;
    narrower.hook.timeout_secs.cleanup -= 1;
    assert!(
        widening(&base(), &narrower).is_empty(),
        "{:?}",
        widening(&base(), &narrower)
    );
    assert_eq!(widening(&narrower, &base()).len(), 17);
}

/// Catches a step that gains a capability being excused because the total shrank: the hook-wide
/// grant goes from `{CAP_KILL}` to nothing while `prepare`, which had it by falling back, now
/// names another capability.
#[test]
fn a_step_that_gains_a_capability_widens_even_when_the_total_shrinks() {
    let mut old = base();
    old.hook.capabilities.insert(cap("CAP_KILL"));
    old.hook.step_capabilities.prepare = None;
    let mut new = old.clone();
    new.hook.capabilities.clear();
    new.hook.step_capabilities.prepare = Some(BTreeSet::from([cap("CAP_SYS_ADMIN")]));
    assert_eq!(widening(&old, &new), ["hook.step_capabilities.prepare"]);
}

/// Catches the fallback being ignored: a step that had its own entry and now has none holds the
/// hook-wide grant, which may be more; and the reverse is narrower when the entry is smaller.
#[test]
fn a_step_without_an_entry_is_compared_by_the_hook_wide_grant() {
    let mut old = base();
    old.hook.capabilities.insert(cap("CAP_KILL"));
    old.hook.step_capabilities.verify = Some(BTreeSet::new());
    let mut new = old.clone();
    new.hook.step_capabilities.verify = None;
    assert_eq!(widening(&old, &new), ["hook.step_capabilities.verify"]);
    assert!(widening(&new, &old).is_empty());
}

/// Catches a new environment variable or parameter being excused because the table is not a set
/// of fixed fields; and a removed one being refused.
#[test]
fn a_new_variable_or_parameter_widens_and_a_removed_one_does_not() {
    let mut more = base();
    more.hook.environment.insert("N".into(), "x".into());
    more.hook.params.insert("extra".into(), true.into());
    assert_eq!(
        widening(&base(), &more),
        ["hook.environment.N", "hook.params.extra"]
    );
    assert!(widening(&more, &base()).is_empty());
}

/// Catches a parameter whose kind changes, or whose text or flag changes, being compared as a
/// number: only a number that falls (or a float that falls) is a lowered number.
#[test]
fn a_parameter_that_is_not_a_lowered_number_widens() {
    let mut text = base();
    text.hook.params.insert("a".into(), "one".into());
    text.hook.params.insert("b".into(), true.into());
    text.hook.params.insert("c".into(), 1.5.into());
    text.hook.params.insert("d".into(), 5.into());
    let widened = |change: &dyn Fn(&mut Declaration)| {
        let mut next = text.clone();
        change(&mut next);
        widening(&text, &next)
    };
    assert_eq!(
        widened(&|d| {
            d.hook.params.insert("a".into(), "two".into());
        }),
        ["hook.params.a"]
    );
    assert_eq!(
        widened(&|d| {
            d.hook.params.insert("b".into(), false.into());
        }),
        ["hook.params.b"]
    );
    assert_eq!(
        widened(&|d| {
            d.hook.params.insert("c".into(), 2.5.into());
        }),
        ["hook.params.c"]
    );
    assert!(
        widened(&|d| {
            d.hook.params.insert("c".into(), 0.5.into());
        })
        .is_empty()
    );
    assert_eq!(
        widened(&|d| {
            d.hook.params.insert("d".into(), 1.5.into());
        }),
        ["hook.params.d"]
    );
    assert_eq!(
        widened(&|d| {
            d.hook.params.insert("d".into(), 6.into());
        }),
        ["hook.params.d"]
    );
    assert!(
        widened(&|d| {
            d.hook.params.insert("d".into(), 4.into());
        })
        .is_empty()
    );
    // NaN is a valid TOML float and is neither above nor below anything: any comparison with
    // it is a widening.
    let mut with_nan = text.clone();
    with_nan.hook.params.insert("c".into(), f64::NAN.into());
    assert_eq!(widening(&text, &with_nan), ["hook.params.c"]);
    let mut huge = with_nan.clone();
    huge.hook.params.insert("c".into(), 1e300.into());
    assert_eq!(widening(&with_nan, &huge), ["hook.params.c"]);
    assert_eq!(widening(&with_nan, &with_nan), ["hook.params.c"]);
}
