//! The text of `kbf-updater --version`.

use super::{version_line, version_text};

/// Catches a `--version` that names no commit: the line is the package version and the stamped
/// commit after a plus sign.
#[test]
fn the_version_line_names_the_stamped_commit() {
    assert_eq!(
        version_text("0.1.0", Some("0123456789ab")),
        "kbf-updater 0.1.0+0123456789ab"
    );
}

/// Catches a build that was not stamped printing nothing or an empty pair of parentheses: it
/// says `unknown`. An empty stamp counts as none.
#[test]
fn an_unstamped_build_says_unknown() {
    assert_eq!(version_text("0.1.0", None), "kbf-updater 0.1.0+unknown");
    assert_eq!(version_text("0.1.0", Some("")), "kbf-updater 0.1.0+unknown");
}

/// Catches the real line drifting from the format: it starts with the package version and its
/// commit is the build's stamp or `unknown`, whichever environment built the test.
#[test]
fn the_real_line_has_the_package_version_then_the_stamp_or_unknown() {
    let want = format!(
        "kbf-updater {}+{}",
        env!("CARGO_PKG_VERSION"),
        option_env!("KBF_BUILD_COMMIT")
            .filter(|c| !c.is_empty())
            .unwrap_or("unknown")
    );
    assert_eq!(version_line(), want);
}
