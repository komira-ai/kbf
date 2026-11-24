//! Unit tests of the `--output` paths a run declares and of the names an output directory's
//! tree may hold. The download itself is tested against the fake farm
//! (`tests/mac_farm/outputs.rs`).

use super::*;

fn paths(list: &[&str]) -> Vec<String> {
    list.iter().map(|p| (*p).to_owned()).collect()
}

/// Catches an output path that is absolute, empty, or climbs out of the checkout (`..`) taken
/// as one: the farm would be asked for a path outside the action's root, and the download
/// would write outside `--out-dir`.
#[test]
fn an_output_that_is_not_a_plain_relative_path_is_refused() {
    for path in ["", "/abs", "a//b", "a/", "./a", "a/../b", ".."] {
        assert_eq!(
            declared(&paths(&[path])),
            Err(Problem::OutputPath(path.to_owned())),
            "{path:?}"
        );
    }
}

/// Catches an output given twice, or one inside another, taken: the farm answers each declared
/// path once, as one file or one directory, so the inner one could never come back on its own.
/// `a-b` sorts between `a` and `a/b`, so a check of neighbours only would miss the last one.
#[test]
fn an_output_given_twice_or_inside_another_is_refused() {
    assert_eq!(
        declared(&paths(&["dist", "target", "dist"])),
        Err(Problem::OutputTwice("dist".to_owned()))
    );
    assert_eq!(
        declared(&paths(&["a/b", "a-b", "a"])),
        Err(Problem::OutputInside {
            inner: "a/b".to_owned(),
            outer: "a".to_owned(),
        })
    );
    assert_eq!(
        declared(&paths(&["ab", "a", "a-b"])),
        Ok(paths(&["a", "a-b", "ab"]))
    );
    assert_eq!(declared(&[]), Ok(Vec::new()));
}

/// Catches an entry name of an output directory that would write outside its directory (a
/// `/`, `..`) or nowhere (empty, `.`) taken as a name.
#[test]
fn an_entry_name_is_one_plain_component() {
    assert!(plain("dist", "kbf.tar.gz").is_ok());
    for name in ["", ".", "..", "a/b", "/a"] {
        let e = plain("dist", name).expect_err("refused");
        assert_eq!(
            e.to_string(),
            format!(
                "the output directory \"dist\" has an entry {name:?}, which is not a plain name"
            )
        );
    }
}
