//! Tests that `setup` refuses the removed `daemon`, `storage-and-daemon` and `all-in-one` modes
//! when its command line is parsed, with the one message that says what replaces them.

use clap::error::ErrorKind;
use clap::{CommandFactory as _, Parser};

use crate::cli::{MODE_REMOVED, Mode, REMOVED_MODES, SetupArgs};

/// The command line of `setup` as `SetupArgs` alone defines it.
#[derive(Parser)]
struct Line {
    #[command(flatten)]
    setup: SetupArgs,
}

fn parse(args: &[&str]) -> Result<SetupArgs, clap::Error> {
    Line::try_parse_from(std::iter::once("setup").chain(args.iter().copied())).map(|l| l.setup)
}

/// Asserts that each removed mode, as one word and as two, fails to parse with the usage exit
/// status 2 and a message that carries [`MODE_REMOVED`] whole, even when the other flags are
/// wrong or missing.
#[test]
fn a_removed_mode_is_refused_with_the_one_message() {
    for word in REMOVED_MODES {
        for args in [
            vec![format!("--mode={word}")],
            vec!["--mode".to_owned(), word.to_owned()],
            vec![
                format!("--mode={word}"),
                "--database=postgres://u@127.0.0.1/db".to_owned(),
            ],
        ] {
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            let Err(e) = parse(&refs) else {
                panic!("{args:?} parsed")
            };
            assert_eq!(e.kind(), ErrorKind::ValueValidation, "{args:?}");
            assert_eq!(e.exit_code(), 2, "{args:?}");
            assert!(e.to_string().contains(MODE_REMOVED), "{args:?}: {e}");
        }
    }
}

/// Asserts that the text names what replaces the modes: the frontend's executor, the storage
/// mode, and the page that says how.
#[test]
fn the_message_names_the_replacement() {
    for word in [
        "removed",
        "no daemon any more",
        "--executor js-agent",
        "--mode=storage",
        "docs/deploy/kbf-modes.md",
    ] {
        assert!(MODE_REMOVED.contains(word), "{word}");
    }
}

/// Asserts that the two modes that remain parse, and that a word that never was a mode is
/// refused with clap's own message, not the removed-mode one.
#[test]
fn the_modes_that_remain_parse_and_an_unknown_word_is_not_called_removed() {
    assert_eq!(
        parse(&["--mode=frontend"]).expect("frontend").mode,
        Mode::Frontend
    );
    assert_eq!(
        parse(&["--mode", "storage"]).expect("storage").mode,
        Mode::Storage
    );
    let Err(e) = parse(&["--mode=node"]) else {
        panic!("node parsed")
    };
    assert!(!e.to_string().contains(MODE_REMOVED), "{e}");
}

/// Asserts that the help of `--mode` names the two modes that remain and none that was removed.
#[test]
fn the_help_of_setup_lists_only_the_modes_that_remain() {
    let help = Line::command().render_long_help().to_string();
    assert!(help.contains("frontend or storage"), "{help}");
    for word in REMOVED_MODES {
        assert!(!help.contains(word), "{word}: {help}");
    }
}
