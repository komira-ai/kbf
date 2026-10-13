//! The connection flags: what is refused before any request, with the flag named.

mod support;

use kbf_bench::connect::{self, ConnectError, Target};

fn target(cas: &str) -> Target {
    Target {
        cas: cas.to_owned(),
        ca_cert: None,
        cert: None,
        key: None,
        tls_server_name: None,
    }
}

fn refused(target: &Target) -> String {
    connect::channel(target).expect_err("refused").to_string()
}

/// Catches a misconfiguration that would only show as every request failing: TLS
/// flags ignored on a plain URL, https without the CA, an unknown scheme, a URL tonic
/// cannot parse, a TLS file missing or not PEM.
#[tokio::test]
async fn bad_connection_flags_are_refused_before_any_request() {
    let server = support::Server::start("connect");
    let mut plain_with_ca = target("http://localhost:1");
    plain_with_ca.ca_cert = Some(server.file("ca.pem"));
    assert!(refused(&plain_with_ca).contains("TLS flags given for a plain-text http:// URL"));
    assert!(refused(&target("ftp://localhost:1")).contains("must be an http:// or https:// URL"));
    assert!(refused(&target("https://localhost:1")).contains("needs --ca-cert"));
    assert!(matches!(
        connect::channel(&target("http://[")),
        Err(ConnectError::Transport(..))
    ));

    let mut no_ca = server.worker_target();
    no_ca.ca_cert = Some(server.file("missing.pem"));
    assert!(
        refused(&no_ca).starts_with("--ca-cert: read "),
        "{}",
        refused(&no_ca)
    );
    (no_ca.cert, no_ca.key) = (None, None);
    assert!(
        refused(&no_ca).starts_with("--ca-cert: read "),
        "{}",
        refused(&no_ca)
    );

    let mut garbage = server.worker_target();
    garbage.cert = Some(server.file("garbage.pem"));
    assert!(matches!(
        connect::channel(&garbage),
        Err(ConnectError::Transport(..))
    ));

    connect::channel(&server.worker_target()).expect("the daemon's flags");
    connect::channel(&server.reapi_target()).expect("plain text");
}
