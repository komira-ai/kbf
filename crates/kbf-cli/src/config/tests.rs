//! The group table of `kbf.toml` against the flags of the modes.

use std::collections::BTreeSet;

use super::*;
use crate::names::MODES;
use crate::own_command;

/// Every setting of every mode: the long names of its own command line.
fn all_settings() -> BTreeSet<String> {
    MODES
        .iter()
        .flat_map(|m| {
            let own = own_command(m.entry);
            let keys: Vec<String> = settings(&own, &crate::refused_own(m.entry))
                .into_keys()
                .collect();
            keys
        })
        .collect()
}

/// Catches a flag added to a mode without its key in the file: every flag of every mode
/// is in a group, so a new flag fails here until `GROUPS` places it (and the golden
/// files, `tests/config_file.rs`, show its key to the reviewer).
#[test]
fn every_flag_of_every_mode_has_a_group() {
    let missing: Vec<String> = all_settings()
        .into_iter()
        .filter(|flag| group_of(flag).is_none())
        .collect();
    assert_eq!(
        missing,
        Vec::<String>::new(),
        "flags with no group in GROUPS"
    );
}

/// Catches a stale or doubled entry of `GROUPS`: a name that no mode has (a renamed flag
/// would leave a key that sets nothing) and a flag in two groups (the second never read).
#[test]
fn the_group_table_names_only_flags_that_exist_once() {
    let all = all_settings();
    let mut seen = BTreeSet::new();
    for (group, flags) in GROUPS {
        for flag in *flags {
            assert!(
                all.contains(*flag),
                "[{group}] names --{flag}: no mode has it"
            );
            assert!(seen.insert(*flag), "--{flag} is in two groups");
        }
    }
}

/// Catches a mode that takes a flag of its own named `--config` or `--check`, which the
/// command line's `--config FILE` and `--check` would collide with (clap refuses the
/// doubled name only when the command is built, so the file loader, which skips both
/// names, would silently not read the mode's).
#[test]
fn no_mode_takes_config_or_check_itself() {
    for m in MODES {
        let own = own_command(m.entry);
        for flag in [crate::CONFIG, crate::CHECK] {
            assert!(
                own.get_arguments().all(|a| a.get_long() != Some(flag)),
                "{} takes --{flag}",
                m.name
            );
        }
    }
}
