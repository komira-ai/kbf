//! Tests of `installer.toml` read by a build without the `proxy` feature, as the `kbf` binary
//! links it (`kbf setup`). Each test says the behaviour it asserts.

use super::*;

/// Asserts that a file with a `[proxy]` table is refused with a problem that begins `proxy:` and
/// names the program that keeps the table, since a build that cannot check the table would drop
/// it when it rewrites the file.
#[test]
fn a_proxy_table_is_refused_naming_the_program_that_keeps_it() {
    let text = "release_source = \"frontend\"\n\n[proxy]\nlisten = \"127.0.0.1:8443\"\n";
    let refused = InstallerToml::parse(text).expect_err("refused").to_string();
    assert!(refused.contains("\nproxy: "), "{refused}");
    assert!(refused.contains("kbf-installer setup"), "{refused}");
}

/// Asserts that a file with no `[proxy]` table is read as it is with the feature: it has no
/// table, no proxy address, and renders none.
#[test]
fn a_file_with_no_proxy_table_has_none_and_renders_none() {
    let parsed = InstallerToml::parse("database = \"postgres:///kbf\"\n").expect("parses");
    assert!(!parsed.has_proxy_table());
    assert!(parsed.proxy_listen().is_none());
    assert!(!parsed.render().contains("[proxy]"));
}
