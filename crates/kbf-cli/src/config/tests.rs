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

fn refusals(text: &str) -> Result<(), String> {
    check_own_tables(std::path::Path::new("/etc/kbf.toml"), text)
}

/// Catches a value where enrollment's table belongs being let through, or refused without
/// naming the key and the file: each of the keys is reported on its own line, in the
/// file's order, with the path.
#[test]
fn a_value_where_an_enrollment_table_belongs_is_refused_naming_key_and_file() {
    let err =
        refusals("node = 1\nstorage = \"http://x\"\ndaemon = true\n[enroll]\ntoken_file = \"t\"\n")
            .unwrap_err();
    let lines: Vec<&str> = err.lines().collect();
    assert_eq!(lines.len(), 3, "{err}");
    for (line, key) in lines.iter().zip(["daemon", "node", "storage"]) {
        assert_eq!(
            *line,
            format!(
                "key `{key}` in /etc/kbf.toml is a value, but [{key}] is the enrollment's table: \
                 it holds keys, not a value"
            )
        );
    }
}

/// Catches the refusal reaching a key that is no table of enrollment (a mode's own flag
/// written outside its table is another check's to report) or a real table of enrollment.
#[test]
fn a_value_outside_enrollments_tables_and_enrollments_tables_themselves_pass() {
    assert_eq!(
        refusals("listen = \"0.0.0.0:1\"\n[node]\nid = \"a\"\n"),
        Ok(())
    );
    assert_eq!(refusals(""), Ok(()));
}

/// Catches the empty group name being taken for a table of enrollment: `KEYS` lists the
/// top-level keys under the empty name, and a quoted empty key is a value like any other
/// stray one, not "the enrollment's table []".
#[test]
fn an_empty_key_is_not_an_enrollment_table() {
    assert_eq!(refusals("\"\" = 1\n"), Ok(()));
}

/// Catches text that is not TOML being refused here: the parser that reads it next names
/// the syntax error, with its line; this check says nothing, so it cannot hide it.
#[test]
fn text_that_is_not_toml_is_left_to_the_parser_that_reads_it_next() {
    assert_eq!(refusals("node = = 1\n[unclosed\n"), Ok(()));
}
