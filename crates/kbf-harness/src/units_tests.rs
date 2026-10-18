//! The unit renderer's wrapper: the mode's spelling, and the edits to a rendered line.

use kbf_deploy::linux::Mode;

use super::*;

/// A rendered unit of `mode` for a made-up host (the example's values, a user id).
fn unit(mode: Mode) -> Rendered {
    rendered(&config(&[]), 1000, mode)
}

#[test]
fn a_unit_names_its_mode_as_its_first_word_and_no_other_spelling_passes() {
    // Catches: a check so loose that a unit naming another mode passes, or that takes
    // `--mode=<name>` anywhere in the line (the rehearsal asserted `exec[1] == mode`; a
    // looser check lets a unit whose first word is something else start a farm of the wrong
    // processes). Mutant: `names_mode` also accepts `--mode=<name>` in any position.
    assert!(names_mode(&["storage", "--cold-store=x"], Mode::Storage));
    assert!(!names_mode(&["storage", "--cold-store=x"], Mode::Daemon));
    assert!(!names_mode(&["--mode=storage"], Mode::Storage));
    assert!(!names_mode(
        &["--cold-store=x", "--mode=storage"],
        Mode::Storage
    ));
    assert!(!names_mode(&["daemon", "--mode=storage"], Mode::Storage));
    // Mutant: the mode name is accepted as any word, not the first.
    assert!(!names_mode(&["--cold-store=x", "storage"], Mode::Storage));
    assert!(!names_mode(&["daemon", "storage"], Mode::Storage));
    assert!(!names_mode(&[], Mode::Storage));
}

#[test]
fn every_mode_renders_a_line_that_names_it() {
    // Catches: a mode whose unit the harness cannot read back (no `ExecStart=`, not
    // `kbf`, another mode's word), which every farm built on it would trip over.
    for mode in Mode::ALL {
        let unit = unit(mode);
        assert_eq!(unit.mode, mode);
        let argv: Vec<&str> = unit.argv.iter().map(String::as_str).collect();
        assert!(names_mode(&argv, mode), "{mode}: {argv:?}");
    }
}

#[test]
fn replace_changes_the_rendered_flag_in_place() {
    // Catches: a replaced flag moved or duplicated (the unit's order is the line's).
    let before = unit(Mode::Daemon);
    let at = before
        .argv
        .iter()
        .position(|a| a.starts_with("--driver="))
        .expect("rendered");
    let after = unit(Mode::Daemon).replace(&["--driver=native".to_owned()]);
    assert_eq!(after.argv.len(), before.argv.len());
    assert_eq!(after.argv[at], "--driver=native");
}

#[test]
#[should_panic(expected = "renders no --no-such-flag")]
fn replace_of_an_unrendered_flag_panics() {
    // Catches: a silent no-op, so a flag the unit stopped rendering leaves the farm
    // running with the unit's own value while the test believes it changed it.
    let _ = unit(Mode::Daemon).replace(&["--no-such-flag=1".to_owned()]);
}

#[test]
#[should_panic(expected = "already renders --driver")]
fn append_of_a_rendered_flag_panics() {
    // Catches: a flag given twice (the later one wins in clap, and the unit's value is
    // then hidden).
    let _ = unit(Mode::Daemon).append(&["--driver=native".to_owned()]);
}

#[test]
fn append_adds_a_flag_the_unit_does_not_render() {
    let after = unit(Mode::Daemon).append(&["--hide-me=1".to_owned()]);
    assert_eq!(after.argv.last().map(String::as_str), Some("--hide-me=1"));
}

#[test]
fn set_replaces_or_adds() {
    // Catches: a limit that is dropped when the unit renders no such flag, or one that is
    // added next to the rendered one.
    let before = unit(Mode::Daemon);
    let driver = before
        .argv
        .iter()
        .filter(|a| a.starts_with("--driver="))
        .count();
    let after = unit(Mode::Daemon).set(&["--driver=native".to_owned(), "--extra=1".to_owned()]);
    assert_eq!(
        after
            .argv
            .iter()
            .filter(|a| a.starts_with("--driver="))
            .count(),
        driver
    );
    assert!(after.argv.contains(&"--driver=native".to_owned()));
    assert_eq!(after.argv.last().map(String::as_str), Some("--extra=1"));
}

#[test]
fn remove_takes_a_rendered_flag_out() {
    let before = unit(Mode::Daemon);
    let after = unit(Mode::Daemon).remove(&["--driver"]);
    assert_eq!(after.argv.len(), before.argv.len() - 1);
    assert!(!after.argv.iter().any(|a| a.starts_with("--driver")));
}

#[test]
#[should_panic(expected = "renders no --no-such-flag")]
fn remove_of_an_unrendered_flag_panics() {
    // Catches: a removal that hides that the unit stopped rendering the flag.
    let _ = unit(Mode::Daemon).remove(&["--no-such-flag"]);
}

#[test]
#[should_panic(expected = "config.example has no")]
fn config_refuses_a_key_the_example_does_not_give() {
    // Catches: a typo in a setting name taken for a new setting nobody reads.
    let _ = config(&[("KBF_NO_SUCH_KEY", "1".to_owned())]);
}

#[test]
fn config_takes_a_setting_over_the_examples() {
    let unit = rendered(
        &config(&[("KBF_NODE_ID", "a-node".to_owned())]),
        1000,
        Mode::Daemon,
    );
    assert!(
        unit.argv.contains(&"--node-id=a-node".to_owned()),
        "{:?}",
        unit.argv
    );
}
