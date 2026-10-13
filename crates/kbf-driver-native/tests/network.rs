//! The network: on unless the action says `network=off`, which the node enforces where
//! it can.

mod support;

use std::net::{IpAddr, TcpListener, UdpSocket};
use std::sync::Arc;

use kbf_driver_native::network::Isolation;
use support::{MemoryCas, Spec, config, run, runtime, scratch, stdout};

/// Perl that connects to `HOST` (an address or a name) on `PORT` and prints the
/// error, or `connected`. A sandbox refuses the connect at once with EPERM; without
/// one, the connect succeeds, times out (the alarm) or fails another way.
const CONNECT: &str = r#"
use Socket;
$SIG{ALRM} = sub { print "timed out\n"; exit 0 };
alarm 10;
my $addr = inet_aton("HOST") or do { print "no address for HOST\n"; exit 0 };
socket(my $s, PF_INET, SOCK_STREAM, 0) or do { print "socket: $!\n"; exit 0 };
if (connect($s, sockaddr_in(PORT, $addr))) { print "connected\n" } else { print "$!\n" }
"#;

/// What the sandbox's refusal prints.
const DENIED: &str = "Operation not permitted";

/// An action that connects to `host` on `port`.
fn connect(host: &str, port: u16) -> Spec {
    Spec::argv(&[
        "/usr/bin/perl",
        "-e",
        &CONNECT
            .replace("HOST", host)
            .replace("PORT", &port.to_string()),
    ])
}

/// This machine's own address on the network it routes to the outside through: the
/// local address of a UDP socket connected to a documentation address (a UDP connect
/// sends nothing). It is not loopback, so the sandbox's loopback allow does not cover
/// it, and a listener bound to it is a server on the node's LAN.
fn lan_address() -> IpAddr {
    let probe = UdpSocket::bind("0.0.0.0:0").expect("bind udp");
    probe
        .connect("192.0.2.1:9")
        .expect("no route out of this machine");
    let ip = probe.local_addr().expect("local addr").ip();
    assert!(!ip.is_loopback() && !ip.is_unspecified(), "{ip}");
    ip
}

/// Catches: the network off by default again (an action that says nothing about the
/// network cannot reach a server on the node's LAN address or a public host),
/// `network=off` no longer keeping it off, `network=on` refused, and loopback refused
/// with the network off (a build's own test servers live there). The LAN server is a
/// listener on this machine's own non-loopback address; the public host is
/// `github.com:443`, which every CI runner reaches. Runs where the node has an
/// isolation mechanism (`sandbox-exec` on macOS); elsewhere it checks the node says it
/// has none.
#[tokio::test]
async fn the_network_is_on_unless_turned_off() {
    let dir = scratch("network");
    let config = config(&dir);
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    if config.isolation == Isolation::None {
        // Every supported Mac has sandbox-exec; a macOS run without it is a failure.
        assert_eq!(std::env::consts::OS, "linux", "no sandbox-exec on this Mac");
        return;
    }
    let lan = lan_address();
    let listener = TcpListener::bind((lan, 0)).expect("bind lan");
    let port = listener.local_addr().expect("addr").port();
    let lan = connect(&lan.to_string(), port);
    let public = connect("github.com", 443);

    let result = run(&rt, &cas, 1, &lan).await.expect("ran");
    assert_eq!(stdout(&cas, &result).trim(), "connected", "default, LAN");
    let result = run(&rt, &cas, 2, &public).await.expect("ran");
    assert_eq!(stdout(&cas, &result).trim(), "connected", "default, public");
    for (seq, value) in [(3, "on"), (4, "standard")] {
        let result = run(&rt, &cas, seq, &lan.clone().property("network", value))
            .await
            .expect("ran");
        assert_eq!(stdout(&cas, &result).trim(), "connected", "{value}");
    }

    for (seq, value, what, spec) in [
        (5, "off", "LAN", &lan),
        (6, "off", "public", &public),
        (7, "none", "LAN", &lan),
        (8, "none", "public", &public),
    ] {
        let spec = spec.clone().property("network", value);
        let result = run(&rt, &cas, seq, &spec).await.expect("ran");
        assert_eq!(stdout(&cas, &result).trim(), DENIED, "{value}, {what}");
    }

    let loopback = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = loopback.local_addr().expect("addr").port();
    let spec = connect("127.0.0.1", port).property("network", "off");
    let result = run(&rt, &cas, 9, &spec).await.expect("ran");
    assert_eq!(stdout(&cas, &result).trim(), "connected", "off, loopback");
}
