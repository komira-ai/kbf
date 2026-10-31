//! The text of `--capabilities` and of the build gauge, and the shape of what the build embeds.

use super::{CAPABILITIES, COMMIT, RELEASE_VERSION, VERSION, capabilities_text, gauge};

/// A commit as the build embeds it: twelve lowercase hex digits, or `unknown` where git gave
/// nothing.
fn is_commit(text: &str) -> bool {
    text == "unknown"
        || (text.len() == 12
            && text
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
}

/// Asserts that each capability is a line of its own that ends in a newline, in the order given,
/// and that none gives no output at all. Catches: names joined by a space or a comma, which a
/// reader that splits on newlines would take for one name, and a missing final newline.
#[test]
fn capabilities_are_printed_one_to_a_line() {
    assert_eq!(capabilities_text(&[]), "");
    assert_eq!(capabilities_text(&["one"]), "one\n");
    assert_eq!(capabilities_text(&["one", "two"]), "one\ntwo\n");
}

/// Asserts that every capability the build lists is a plain lowercase word with `-` and digits,
/// and is listed once. Catches: a name with a space or a newline in it, which would print as two
/// capabilities or as none, and one named twice.
#[test]
fn the_capability_names_are_plain_and_unique() {
    for name in CAPABILITIES {
        let plain = !name.is_empty()
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        assert!(plain, "{name:?}");
    }
    let mut sorted = CAPABILITIES.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), CAPABILITIES.len(), "{CAPABILITIES:?}");
}

/// Asserts the text of the gauge: a `TYPE` line, then one sample of value 1 with the version and
/// the commit as labels, in that order, each line ending in a newline. Catches: a gauge that has
/// no `TYPE` line, one with another value, or labels left out or swapped.
#[test]
fn the_gauge_is_one_sample_with_version_and_commit() {
    assert_eq!(
        gauge("1.2.3", "0123456789ab"),
        "# TYPE tierstore_build_info gauge\n\
         tierstore_build_info{version=\"1.2.3\",commit=\"0123456789ab\"} 1\n"
    );
}

/// Asserts that the commit the build embeds is twelve lowercase hex digits or `unknown`, and that
/// the version `--version` prints is the crate's version, `+`, and that commit: the form the
/// release check reads. Catches: an empty value (the build script's variable not read), a
/// placeholder that is not a commit, and a version string that leaves the commit out or joins
/// it another way.
#[test]
fn the_build_embeds_a_commit_and_the_version_names_it() {
    assert!(is_commit(COMMIT), "{COMMIT:?}");
    assert_eq!(VERSION, env!("CARGO_PKG_VERSION"));
    assert_eq!(RELEASE_VERSION, format!("{VERSION}+{COMMIT}"));
}
