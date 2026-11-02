//! Widening: does a new declaration ask for more than the one it replaces?
//!
//! **Widening is any change other than removing an element from a set or lowering a number**,
//! in every field (docs/design/host-updates.md, section 3.3). So:
//!
//! - a set (capabilities, paths, units, families, ports, environment names and parameter
//!   names) widens when it gains an element;
//! - a number (a timeout, a deadline, a retry count, a registration lifetime) widens when it
//!   rises; the promotion ceiling widens when it rises or goes away, because no ceiling is the
//!   highest one;
//! - a capability set is compared **per step**, on what the step ends up holding (its own
//!   entry, or the hook-wide grant when it has none), so a step that gains a capability widens
//!   even when the hook-wide grant shrinks;
//! - everything else (the signer, the accounts, the binary, its arguments, the record
//!   directory, the boot entry point, the first release, the value of a variable) widens when it
//!   changes at all. `signer`, `hook.user` and `binary` are among them, so they always need
//!   approval.
//!
//! The comparison names every field by destructuring both declarations, so a field added to
//! [`Declaration`] does not compile here until it is compared: a field the comparer does not know
//! cannot be skipped.

use std::collections::BTreeSet;

use update_hook::Step;

use crate::declaration::{Declaration, Hook, Signer};

fn grew<T: Ord>(old: &BTreeSet<T>, new: &BTreeSet<T>) -> bool {
    !new.is_subset(old)
}

fn raised(old: u64, new: u64) -> bool {
    new > old
}

fn changed<T: PartialEq>(old: &T, new: &T) -> bool {
    old != new
}

/// A ceiling that is absent is no ceiling, the highest of all.
fn ceiling_raised(old: Option<u64>, new: Option<u64>) -> bool {
    raised(old.unwrap_or(u64::MAX), new.unwrap_or(u64::MAX))
}

/// An optional text widens when it appears or changes; removing it narrows.
fn text_gained(old: &Option<String>, new: &Option<String>) -> bool {
    new.as_ref().is_some_and(|new| old.as_ref() != Some(new))
}

/// For each key of `new`: whether it is absent from `old` or `widened` says its value grew.
fn keys_widened<'a, V: 'a>(
    prefix: &str,
    new: impl Iterator<Item = (&'a String, &'a V)>,
    old_value: impl Fn(&str) -> Option<&'a V>,
    widened: impl Fn(&V, &V) -> bool,
) -> Vec<(String, bool)> {
    new.map(|(key, value)| {
        let wider = old_value(key).is_none_or(|before| widened(before, value));
        (format!("{prefix}.{key}"), wider)
    })
    .collect()
}

fn param_widened(old: &toml::Value, new: &toml::Value) -> bool {
    match (old, new) {
        (toml::Value::Integer(old), toml::Value::Integer(new)) => new > old,
        // `nan` is a valid TOML float and compares as neither above nor below: not lower is wider.
        (toml::Value::Float(old), toml::Value::Float(new)) => matches!(
            new.partial_cmp(old),
            Some(std::cmp::Ordering::Greater) | None
        ),
        (old, new) => old != new,
    }
}

/// The fields of `new` that ask for more than `old` allowed, named as they are in the file
/// (`hook.write_paths`, `hook.timeout_secs.verify`, `hook.environment.NAME`). Empty when `new`
/// is equal or narrower.
pub fn widening(old: &Declaration, new: &Declaration) -> Vec<String> {
    top_level(old, new)
        .into_iter()
        .chain(hook_level(&old.hook, &new.hook))
        .filter(|(_, widened)| *widened)
        .map(|(field, _)| field)
        .collect()
}

/// The comparisons of the top-level keys. Every field is named in both patterns, so a field
/// added to the type does not compile until it is compared.
fn top_level(old: &Declaration, new: &Declaration) -> Vec<(String, bool)> {
    let Declaration {
        name: old_name,
        binary: old_binary,
        hook_args: old_hook_args,
        signer:
            Signer {
                repository: old_repository,
                workflow: old_workflow,
            },
        uid: old_uid,
        registration_ttl_secs: old_ttl,
        health_ports: old_ports,
        verify_deadline_secs: old_deadline,
        promotion_ceiling_secs: old_ceiling,
        cleanup_retries: old_retries,
        state_dir: old_state_dir,
        boot: old_boot,
        min_version: old_min_version,
        hook: _,
    } = old;
    let Declaration {
        name: new_name,
        binary: new_binary,
        hook_args: new_hook_args,
        signer:
            Signer {
                repository: new_repository,
                workflow: new_workflow,
            },
        uid: new_uid,
        registration_ttl_secs: new_ttl,
        health_ports: new_ports,
        verify_deadline_secs: new_deadline,
        promotion_ceiling_secs: new_ceiling,
        cleanup_retries: new_retries,
        state_dir: new_state_dir,
        boot: new_boot,
        min_version: new_min_version,
        hook: _,
    } = new;
    vec![
        ("name".into(), changed(old_name, new_name)),
        ("binary".into(), changed(old_binary, new_binary)),
        ("hook_args".into(), changed(old_hook_args, new_hook_args)),
        (
            "signer.repository".into(),
            changed(old_repository, new_repository),
        ),
        (
            "signer.workflow".into(),
            changed(old_workflow, new_workflow),
        ),
        ("uid".into(), changed(old_uid, new_uid)),
        ("registration_ttl_secs".into(), raised(*old_ttl, *new_ttl)),
        ("health_ports".into(), grew(old_ports, new_ports)),
        (
            "verify_deadline_secs".into(),
            raised(*old_deadline, *new_deadline),
        ),
        (
            "promotion_ceiling_secs".into(),
            ceiling_raised(*old_ceiling, *new_ceiling),
        ),
        (
            "cleanup_retries".into(),
            raised(u64::from(*old_retries), u64::from(*new_retries)),
        ),
        ("state_dir".into(), changed(old_state_dir, new_state_dir)),
        ("boot".into(), text_gained(old_boot, new_boot)),
        (
            "min_version".into(),
            changed(old_min_version, new_min_version),
        ),
    ]
}

/// The comparisons of the `[hook]` table, by the same rule.
fn hook_level(old: &Hook, new: &Hook) -> Vec<(String, bool)> {
    let Hook {
        user: old_user,
        group: old_group,
        capabilities: old_capabilities,
        step_capabilities: _,
        read_paths: old_read,
        write_paths: old_write,
        units: old_units,
        address_families: old_families,
        environment: old_environment,
        timeout_secs: old_timeouts,
        params: old_params,
    } = old;
    let Hook {
        user: new_user,
        group: new_group,
        capabilities: new_capabilities,
        step_capabilities: _,
        read_paths: new_read,
        write_paths: new_write,
        units: new_units,
        address_families: new_families,
        environment: new_environment,
        timeout_secs: new_timeouts,
        params: new_params,
    } = new;
    let mut named = vec![
        ("hook.user".into(), changed(old_user, new_user)),
        ("hook.group".into(), changed(old_group, new_group)),
        (
            "hook.capabilities".into(),
            grew(old_capabilities, new_capabilities),
        ),
        ("hook.read_paths".into(), grew(old_read, new_read)),
        ("hook.write_paths".into(), grew(old_write, new_write)),
        ("hook.units".into(), grew(old_units, new_units)),
        (
            "hook.address_families".into(),
            grew(old_families, new_families),
        ),
    ];
    // Per step, on what the step ends up holding.
    named.extend(Step::ALL.iter().map(|step| {
        (
            format!("hook.step_capabilities.{step}"),
            grew(old.capabilities_for(*step), new.capabilities_for(*step)),
        )
    }));
    named.extend(Step::ALL.iter().map(|step| {
        (
            format!("hook.timeout_secs.{step}"),
            raised(old_timeouts.get(*step), new_timeouts.get(*step)),
        )
    }));
    named.extend(keys_widened(
        "hook.environment",
        new_environment.iter(),
        |key| old_environment.get(key),
        |before, value| before != value,
    ));
    named.extend(keys_widened(
        "hook.params",
        new_params.iter(),
        |key| old_params.get(key),
        param_widened,
    ));
    named
}
