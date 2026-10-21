use super::*;
use crate::floors;

fn floors_of(text: &str) -> Floors {
    floors::parse(text).expect("floors")
}

fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(new, old)| ((*new).to_owned(), (*old).to_owned()))
        .collect()
}

/// Catches: a name read from another section (a dependency called `name`), a quoted
/// value not closed, a manifest with no package name.
#[test]
fn package_name_reads_the_package_section_only() {
    let toml = "[dependencies]\nname = \"dep\"\n[package]\nversion = \"1\"\nname = \"kbf-x\" # c\n";
    assert_eq!(package_name(toml).as_deref(), Some("kbf-x"));
    assert_eq!(
        package_name("[package]\nname=\"y\"\n").as_deref(),
        Some("y")
    );
    assert_eq!(package_name("[package]\nname.workspace = true\n"), None);
    assert_eq!(package_name("[package]\nname = \"open\n"), None);
    assert_eq!(package_name("[lib]\nname = \"z\"\n"), None);
    assert_eq!(package_name(""), None);
}

/// Catches: a modified, added or deleted manifest read as a move; a rename of a file
/// that is not `crates/<dir>/Cargo.toml` (a nested one, a source file) read as a crate
/// move; a parser that loses its place after a one-path record or a short record.
#[test]
fn manifest_moves_reads_only_crate_manifest_renames() {
    let raw = b"M\0crates/a/Cargo.toml\0R100\0crates/o/Cargo.toml\0crates/n/Cargo.toml\0\
                A\0crates/q/Cargo.toml\0D\0crates/r/Cargo.toml\0\
                R090\0crates/o/src/lib.rs\0crates/n/src/lib.rs\0\
                R100\0crates/o/x/Cargo.toml\0crates/n/x/Cargo.toml\0\
                R100\0crates/p/Cargo.toml\0crates/s/Cargo.toml\0";
    assert_eq!(
        manifest_moves(raw),
        [("o".into(), "n".into()), ("p".into(), "s".into())]
    );
    assert_eq!(manifest_moves(b""), []);
    assert_eq!(manifest_moves(b"R100\0crates/o/Cargo.toml\0"), []);
}

/// Catches: a directory move under the same package name, or a manifest git cannot
/// show, read as a rename.
#[test]
fn only_a_changed_package_name_is_a_rename() {
    let moves = [("a".to_owned(), "b".to_owned())];
    let names = |base: Option<&'static str>, head: Option<&'static str>| {
        renamed_crates(&moves, move |side, dir| {
            let (want, name) = match side {
                Side::Base => ("a", base),
                Side::Head => ("b", head),
            };
            assert_eq!(dir, want);
            name.map(str::to_owned)
        })
    };
    assert_eq!(names(Some("x"), Some("y")), map(&[("b", "a")]));
    assert_eq!(names(Some("x"), Some("x")), map(&[]));
    assert_eq!(names(None, Some("y")), map(&[]));
    assert_eq!(names(Some("x"), None), map(&[]));
}

/// Catches (kbf#1401): a floor moved with its crate read as a floor added; a rename
/// that also lowers a floor passing; a really new crate with a floor passing; a
/// removed crate failing; a rename map that touches another crate's row.
#[test]
fn a_renamed_crate_keeps_its_base_floor() {
    let base = floors_of("old - 97.43\nkeep 90.00 80.00\n");
    let renames = map(&[("new", "old")]);
    let shown = |head: &str| -> Vec<String> {
        lowered_renamed(&base, &floors_of(head), &renames)
            .iter()
            .map(ToString::to_string)
            .collect()
    };
    let none: [&str; 0] = [];
    assert_eq!(shown("new - 97.43\nkeep 90.00 80.00\n"), none);
    assert_eq!(shown("new - 98.00\nkeep 90.00 80.00\n"), none);
    assert_eq!(
        shown("new - 97.42\nkeep 90.00 80.00\n"),
        ["new branches: threshold 97.43 -> 97.42 (a floor may only rise)"]
    );
    assert_eq!(
        shown("new 50.00 97.43\nkeep 90.00 80.00\n"),
        [
            "new lines: threshold 99.00 -> 50.00 (a floor was added; a crate under 99% needs tests, not a floor)"
        ]
    );
    // The rename dropped its floor (the crate reached 99%): fine.
    assert_eq!(shown("keep 90.00 80.00\n"), none);
    // A crate that is really new still defaults to 99.00.
    assert_eq!(
        shown("new - 97.43\nkeep 90.00 80.00\nfresh - 10.00\n"),
        [
            "fresh branches: threshold 99.00 -> 10.00 (a floor was added; a crate under 99% needs tests, not a floor)"
        ]
    );
    // A rename whose old crate had no floor leaves the new one at 99.00.
    let renames = map(&[("new", "nofloor")]);
    assert_eq!(
        lowered_renamed(
            &base,
            &floors_of("old - 97.43\nkeep 90.00 80.00\nnew - 10.00\n"),
            &renames
        )
        .len(),
        1
    );
    // Without renames this is `lowered`.
    assert_eq!(
        lowered_renamed(
            &base,
            &floors_of("new - 97.43\nkeep 90.00 80.00\n"),
            &map(&[])
        )
        .len(),
        1
    );
}
