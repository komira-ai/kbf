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

/// Catches the file template depending on the shape of a flag: a bare setting (no help,
/// no value name) is a commented line holding its own upper-cased name; help is a comment
/// per line, a hidden flag says so; a switch is live with `false`; a repeated flag with
/// defaults is a list; a number, an integer or a float, is written as itself, and a
/// default that only parses as a non-finite float (`inf`) is a string, since TOML has no
/// such number. One command holds them all so a single test binary reaches both sides of
/// every branch of the entry.
#[test]
fn the_template_shows_each_shape_of_flag() {
    use clap::{Arg, ArgAction};
    let own = clap::Command::new("t")
        .arg(Arg::new("listen").long("listen"))
        .arg(
            Arg::new("storage")
                .long("storage")
                .help("first line\nsecond line")
                .default_value("1.5"),
        )
        .arg(
            Arg::new("heartbeat-interval-ms")
                .long("heartbeat-interval-ms")
                .hide(true)
                .default_value("inf"),
        )
        .arg(
            Arg::new("worker-listen")
                .long("worker-listen")
                .value_name("ADDR")
                .default_value("7"),
        )
        .arg(
            Arg::new("state-store")
                .long("state-store")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("worker-deny-list")
                .long("worker-deny-list")
                .action(ArgAction::Append)
                .default_values(["a", "b\"c"]),
        );
    let text = template("t", &own, &BTreeSet::new());
    for line in [
        "# listen = \"LISTEN\"\n",
        "# first line\n# second line\nstorage = 1.5\n",
        "# (not in --help)\nheartbeat-interval-ms = \"inf\"\n",
        "worker-listen = 7\n",
        "state-store = false\n",
        "worker-deny-list = [\"a\", \"b\\\"c\"]\n",
    ] {
        assert!(text.contains(line), "{line:?} missing from\n{text}");
    }
}

/// Catches a flag in no group being given a quiet default table: the template of a command
/// holding a flag `GROUPS` does not place panics, naming the flag (the unit tests above
/// keep that from reaching a real mode).
#[test]
#[should_panic(expected = "--zz-ungrouped is in no group")]
fn the_template_refuses_a_flag_in_no_group() {
    let own = clap::Command::new("t").arg(clap::Arg::new("zz-ungrouped").long("zz-ungrouped"));
    let _ = template("t", &own, &BTreeSet::new());
}
