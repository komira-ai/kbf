//! What this build says of itself: its version, the commit it was built from, and the
//! capabilities a host updater may ask for with `tierstore --capabilities`.

/// The crate version, which `--version`, the start line and `tierstore_build_info` name.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The commit this binary was built from.
pub const COMMIT: &str = "unknown";

/// The capabilities `tierstore --capabilities` prints.
pub const CAPABILITIES: &[&str] = &[];

/// The text `--capabilities` prints for `names`.
#[must_use]
pub fn capabilities_text(_names: &[&str]) -> String {
    String::new()
}

/// The `tierstore_build_info` block of `/metricz`.
#[must_use]
pub fn gauge(_version: &str, _commit: &str) -> String {
    String::new()
}

#[cfg(test)]
#[path = "build_info_tests.rs"]
mod tests;
