//! The table of modes (`kbf_cli::names`) against everything that spells a mode as text.

use clap::ValueEnum;
use kbf_cli::names::{self, MODES};
use kbf_installer::cli::Mode;

/// Catches a CLI whose modes are not exactly the table's (a renamed constant with a
/// literal left behind, or a mode added to one and not the other), and a constant
/// that is not in the table.
#[test]
fn the_cli_has_exactly_the_tables_modes() {
    let cmd = kbf_cli::command();
    let have: Vec<&str> = cmd.get_subcommands().map(clap::Command::get_name).collect();
    let want: Vec<&str> = MODES.iter().map(|m| m.name).collect();
    assert_eq!(have, want);
    let constants = [
        names::FRONTEND,
        names::STORAGE,
        names::DAEMON,
        names::STORAGE_AND_DAEMON,
        names::ALL_IN_ONE,
    ];
    assert_eq!(constants.to_vec(), want);
}

/// Asserts that exactly the frontend and all-in-one keep their state in the database, and that
/// a word that is not a mode does not.
#[test]
fn only_the_frontend_and_all_in_one_keep_their_state_in_the_database() {
    let keeping: Vec<&str> = MODES
        .iter()
        .map(|m| m.name)
        .filter(|n| names::keeps_state_in_database(n))
        .collect();
    assert_eq!(keeping, [names::FRONTEND, names::ALL_IN_ONE]);
    assert!(!names::keeps_state_in_database("node"));
}

/// Asserts that the installer's modes are the table's modes, and that each keeps its state in
/// the database exactly when the table says so: the installer's `setup` and `kbf boot-check`
/// then agree on which units need the database.
#[test]
fn the_installers_modes_keep_state_in_the_database_as_the_table_says() {
    let modes = Mode::value_variants();
    assert_eq!(modes.len(), MODES.len());
    for mode in modes {
        let value = mode.to_possible_value().expect("a named mode");
        let name = value.get_name();
        assert!(MODES.iter().any(|m| m.name == name), "{name}");
        let kept = names::keeps_state_in_database(name);
        assert_eq!(kept, mode.keeps_state_in_database(), "{name}");
    }
}

/// Catches a mode whose help says it is not built, though every mode runs.
#[test]
fn no_modes_help_says_it_is_not_built() {
    let cmd = kbf_cli::command();
    for mode in MODES {
        let about = cmd
            .find_subcommand(mode.name)
            .expect("mode")
            .get_about()
            .map(ToString::to_string)
            .unwrap_or_default();
        assert!(!about.contains("Not built"), "{}: {about}", mode.name);
    }
}
