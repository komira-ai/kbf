//! What this build says of itself: its version, the commit it was built from, and the
//! capabilities a host updater may ask for with `tierstore --capabilities`.

/// The crate version, which `--version`, the start line and `tierstore_build_info` name.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The commit this binary was built from: twelve hex digits, or `unknown` where the build had no
/// git checkout. `build.rs` embeds it.
pub const COMMIT: &str = env!("TIERSTORE_BUILD_COMMIT");

/// What `--version` prints after the program's name.
pub const RELEASE_VERSION: &str = VERSION;

/// The capabilities `tierstore --capabilities` prints, one to a line, in this order. A name is
/// lowercase letters, digits and `-`.
///
/// **This list is empty on purpose, and it must not name `handover-v1` yet.** `handover-v1` says
/// the release has the lock-based read-only mode, the lingering close in the drain and the
/// wildcard listeners; the host updater's `prepare` step reads it to choose a handover over a
/// plain restart, so a release that names it without all three would be handed over to and fail.
/// Those three arrive in other changes, and the one that completes them adds `handover-v1` here,
/// in this one place and nowhere else.
pub const CAPABILITIES: &[&str] = &[];

/// The text `--capabilities` prints for `names`: each name and a newline, nothing for none.
#[must_use]
pub fn capabilities_text(names: &[&str]) -> String {
    names.iter().map(|name| format!("{name}\n")).collect()
}

/// The `tierstore_build_info` block of `/metricz`: the type line and one sample of value 1 that
/// carries the version and the commit as labels. Neither can hold a quote or a backslash: one is
/// a Cargo version, the other hex digits or `unknown`.
#[must_use]
pub fn gauge(version: &str, commit: &str) -> String {
    format!(
        "# TYPE tierstore_build_info gauge\n\
         tierstore_build_info{{version=\"{version}\",commit=\"{commit}\"}} 1\n"
    )
}

#[cfg(test)]
#[path = "build_info_tests.rs"]
mod tests;
