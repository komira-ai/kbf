use std::fs;

use super::*;

/// A pool table over files that are not there, which is enough to fail a start.
const POOL: &str = r#"
operator_token_file = "/etc/kbf/operator.token"
server_cert_file = "/etc/kbf/tls/server.pem"
server_key_file = "/etc/kbf/tls/server.key"
client_ca_file = "/etc/kbf/tls/ca.pem"
[[frontend]]
name = "a"
base_port = 18000
[[frontend]]
name = "b"
base_port = 18010
"#;

/// Asserts that a start the proxy cannot make (a descriptor that is not a socket, files that
/// are not there) is a failure that says so in the proxy's words, and that the signal hook is
/// not run for a proxy that never served.
#[test]
fn a_proxy_that_cannot_be_bound_is_a_failure_to_start() {
    let config = ProxyConfig::from_toml_str(POOL).expect("a valid pool table");
    let regular = OwnedFd::from(fs::File::open("/dev/null").expect("open"));
    let mut hooked = false;
    let error = run(config, regular, || hooked = true).expect_err("no proxy");
    let text = error.to_string();
    assert!(text.starts_with("cannot start the proxy: "), "{text}");
    assert!(!hooked, "the hook waits for the bind");
}
