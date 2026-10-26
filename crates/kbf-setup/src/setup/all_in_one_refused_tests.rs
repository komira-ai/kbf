//! Tests that `setup` refuses the removed `all-in-one` mode before it looks at the host.

use clap::Parser;

use super::cli;
use crate::cli::{ALL_IN_ONE_REMOVED, SetupArgs};
use crate::exit::Exit;

/// The command line of `setup` as `SetupArgs` alone defines it.
#[derive(Parser)]
struct Line {
    #[command(flatten)]
    setup: SetupArgs,
}

fn line(args: &[&str]) -> SetupArgs {
    Line::try_parse_from(std::iter::once("setup").chain(args.iter().copied()))
        .expect("a command line")
        .setup
}

/// Asserts that `--mode=all-in-one` (as one word and as two, with and without `--apply`) fails
/// with the message that names the replacement and the usage exit status, prints no step, and
/// leaves the host root it was given empty, even when the other flags are missing or wrong.
#[test]
fn all_in_one_is_refused_with_the_message_before_anything_is_written() {
    for (args, apply) in [
        (&["--mode=all-in-one"][..], false),
        (&["--mode", "all-in-one"], false),
        (&["--mode=all-in-one"], true),
        (
            &["--mode=all-in-one", "--database=postgres://u@127.0.0.1/db"],
            true,
        ),
    ] {
        let root = kbf_testkit::dir::TestDir::under(std::env::temp_dir(), "kbf-setup-aio");
        let mut steps = Vec::new();
        let e = cli(root.path().to_path_buf(), &line(args), apply, &mut steps)
            .expect_err("the mode is refused");
        assert_eq!(e.to_string(), ALL_IN_ONE_REMOVED, "{args:?}");
        assert_eq!(e.exit(), Exit::Usage, "{args:?}");
        assert!(
            steps.is_empty(),
            "{args:?}: {}",
            String::from_utf8_lossy(&steps)
        );
        let left: Vec<_> = std::fs::read_dir(root.path()).expect("root").collect();
        assert!(left.is_empty(), "{args:?}: {left:?}");
    }
}

/// Asserts that the text names both nodes that replace the mode and the page that says how.
#[test]
fn the_message_names_the_replacement() {
    for word in [
        "removed",
        "--mode=frontend",
        "--mode=storage-and-daemon",
        "docs/deploy/kbf-modes.md",
    ] {
        assert!(ALL_IN_ONE_REMOVED.contains(word), "{word}");
    }
}

/// Asserts that the listed values of `--mode` no longer include `all-in-one`, while the other
/// four are still listed.
#[test]
fn the_help_of_setup_does_not_list_all_in_one() {
    let help = {
        use clap::CommandFactory as _;
        Line::command().render_long_help().to_string()
    };
    assert!(
        help.contains("[possible values: frontend, storage, daemon, storage-and-daemon]"),
        "{help}"
    );
    for mode in ["frontend", "storage", "daemon", "storage-and-daemon"] {
        assert!(help.contains(mode), "{mode}: {help}");
    }
}
