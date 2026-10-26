//! The `[proxy]` table in pool mode (`[[proxy.frontend]]` tables and `admin_listen`) through
//! `render`, which `setup` rewrites `installer.toml` with.

use super::*;

const POOL: &str = r#"
[proxy]
operator_token_file = "/etc/kbf/operator.token"
server_cert_file = "/etc/kbf/tls/server.pem"
server_key_file = "/etc/kbf/tls/server.key"
client_ca_file = "/etc/kbf/tls/ca.pem"
admin_listen = "127.0.0.1:18090"

[[proxy.frontend]]
name = "a"
base_port = 18000

[[proxy.frontend]]
name = "b"
base_port = 18010
"#;

/// Asserts that a pool table is read with its frontends and admin address, that it renders
/// with them and without a slot table, and that what it renders reads back as the same table
/// and renders the same text again: a `setup` run that rewrites the file keeps the pool.
#[test]
fn the_pool_tables_survive_a_render() {
    let parsed = InstallerToml::parse(POOL).expect("parses");
    let proxy = parsed.proxy.as_ref().expect("a proxy table");
    assert_eq!(proxy.upstream, None);
    assert_eq!(proxy.frontend.len(), 2);
    let text = parsed.render();
    assert!(!text.contains("[proxy.upstream]"), "{text}");
    let admin = "admin_listen = \"127.0.0.1:18090\"\n";
    assert!(text.contains(admin), "{text}");
    let second = "\n[[proxy.frontend]]\nname = \"b\"\nbase_port = 18010\n";
    assert!(text.contains(second), "{text}");
    let again = InstallerToml::parse(&text).expect("parses back");
    assert_eq!(again, parsed);
    assert_eq!(again.render(), text);
}

/// Asserts that a table with the slot table of the removed two-slot supervision, beside
/// frontends or alone, is read when the file is read and renders with the slot table kept, so
/// that `setup` and the release fetcher go on reading a file written before the removal, and
/// that the check the proxy makes at its start refuses it in the words that say what replaced
/// it.
#[test]
fn a_table_with_the_slot_table_is_read_and_kept_and_refused_only_by_the_proxy_check() {
    let beside = format!("{POOL}\n[proxy.upstream]\nbase_port = 19000\n");
    let alone = POOL.replace(
        "\n[[proxy.frontend]]\nname = \"a\"\nbase_port = 18000\n\n\
         [[proxy.frontend]]\nname = \"b\"\nbase_port = 18010\n",
        "\n[proxy.upstream]\nbase_port = 18000\n",
    );
    assert_ne!(alone, POOL);
    for text in [beside, alone] {
        let parsed = InstallerToml::parse(&text).expect("read");
        let proxy = parsed.proxy.as_ref().expect("a proxy table");
        assert!(proxy.upstream.is_some(), "{text}");
        let rendered = parsed.render();
        assert!(
            rendered.contains("\n[proxy.upstream]\nbase_port = "),
            "{rendered}"
        );
        assert_eq!(InstallerToml::parse(&rendered).expect("again"), parsed);
        let why = proxy.check_pool().expect_err("refused").to_string();
        assert!(why.contains("two-slot supervision was removed"), "{why}");
        assert!(why.contains("[[proxy.frontend]]"), "{why}");
    }
}
