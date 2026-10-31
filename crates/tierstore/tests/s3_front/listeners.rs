//! Repeated `--listen` and `--oci-listen`: one server over every address, in this process and as
//! the binary.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use clap::Parser;
use reqwest::Client;
use rustix::process::Signal;
use tierstore::server::Drained;
use tierstore::server::cli::{Addresses, Cli, run_serving};
use tokio::net::TcpStream;
use tokio::sync::oneshot;

use crate::binary::start;
use crate::files::Files;

/// Whether this host has IPv6 loopback, decided by trying to bind it.
fn has_v6() -> bool {
    std::net::TcpListener::bind("[::1]:0").is_ok()
}

/// The address a client dials to reach a socket bound at `at`: a wildcard by way of loopback.
fn reach(at: SocketAddr) -> SocketAddr {
    let ip = match at.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    SocketAddr::new(ip, at.port())
}

/// A client that keeps no connection after an answer, so that nothing of it is open at the drain.
fn client() -> Client {
    Client::builder().pool_max_idle_per_host(0).build().unwrap()
}

fn parse(flags: Vec<String>) -> Cli {
    let argv = std::iter::once("tierstore".to_owned()).chain(flags);
    Cli::try_parse_from(argv).unwrap()
}

/// How many times `flag` is in `flags`.
fn given(flags: &[String], flag: &str) -> usize {
    flags.iter().filter(|word| *word == flag).count()
}

/// Asserts that `run_serving` binds a socket for each `--listen` and each `--oci-listen`, says
/// every address to `ready` (the first as `s3` and `oci`), serves the S3 and the registry calls on
/// all of them, and that the stop ends the one drain with nothing cut and closes every socket.
#[tokio::test]
async fn every_listen_flag_is_served_and_drained_together() {
    let files = Files::new("listeners-all");
    let mut extra = vec![
        "--drain-lb-wait",
        "50ms",
        "--listen",
        "0.0.0.0:0",
        "--oci-listen",
        "127.0.0.1:0",
        "--oci-listen",
        "0.0.0.0:0",
    ];
    if has_v6() {
        extra.extend(["--listen", "[::1]:0", "--oci-listen", "[::1]:0"]);
    }
    let flags = files.flags(&extra);
    let (s3_asked, oci_asked) = (given(&flags, "--listen"), given(&flags, "--oci-listen"));
    let cli = parse(flags);
    let (stop, stopped) = oneshot::channel::<()>();
    let (told, bound) = oneshot::channel::<Addresses>();
    let running = run_serving(
        &cli,
        async move {
            let _ = stopped.await;
        },
        move |bound| {
            let _ = told.send(bound);
        },
    );
    let driving = async {
        let bound = bound.await.unwrap();
        assert_eq!(bound.s3_all.len(), s3_asked, "{bound:?}");
        assert_eq!(bound.oci_all.len(), oci_asked, "{bound:?}");
        assert_eq!(bound.s3, bound.s3_all[0]);
        assert_eq!(bound.oci, Some(bound.oci_all[0]));
        let client = client();
        for at in &bound.s3_all {
            let answer = client.get(format!("http://{}/healthz", reach(*at)));
            let status = answer.send().await.unwrap().status();
            assert_eq!(status, 200, "S3 at {at}");
        }
        for at in &bound.oci_all {
            let answer = client.get(format!("http://{}/v2/", reach(*at)));
            let status = answer.send().await.unwrap().status();
            assert_eq!(status, 200, "OCI at {at}");
        }
        stop.send(()).unwrap();
        bound
    };
    let (drained, bound) = tokio::join!(running, driving);
    assert_eq!(drained.unwrap(), Drained { abandoned: 0 });
    for at in bound.s3_all.iter().chain(&bound.oci_all) {
        let refused = TcpStream::connect(reach(*at)).await;
        assert!(refused.is_err(), "{at} still takes connections");
    }
}

/// Asserts that an address given twice is refused before anything is bound or served, for
/// `--listen` and for `--oci-listen`, with the flag in the message and the address not in it.
#[tokio::test]
async fn an_address_given_twice_is_refused() {
    let files = Files::new("listeners-twice");
    // `Files::flags` has `--listen 127.0.0.1:0` already.
    let again = ["--listen", "127.0.0.1:0"];
    let twice = ["--oci-listen", "127.0.0.1:0", "--oci-listen", "127.0.0.1:0"];
    for (flag, extra) in [("--listen", &again[..]), ("--oci-listen", &twice[..])] {
        let mut words = vec!["--drain-lb-wait", "50ms"];
        words.extend(extra);
        let cli = parse(files.flags(&words));
        let refused = run_serving(&cli, async {}, |_| {}).await.err();
        let why = refused.expect("a refused start").to_string();
        assert!(why.contains(flag), "{why}");
        assert!(why.contains("the same address twice"), "{why}");
        assert!(!why.contains("127.0.0.1"), "{why}");
    }
}

/// Asserts that the binary's start line names every address of the S3 front, separated by
/// commas in the one word `listen=`, that each of them answers, and that SIGTERM ends it with
/// status 0.
#[tokio::test]
async fn the_start_line_names_every_address() {
    let files = Files::new("listeners-line");
    let flags = files.flags(&["--listen", "0.0.0.0:0", "--drain-lb-wait", "50ms"]);
    let mut running = start(&flags);
    let named: Vec<SocketAddr> = running
        .addr()
        .split(',')
        .map(|word| word.parse().expect("an address"))
        .collect();
    assert_eq!(named.len(), 2, "{}", running.addr());
    let client = client();
    for at in &named {
        assert_ne!(at.port(), 0, "{at}");
        let answer = client.get(format!("http://{}/healthz", reach(*at)));
        assert_eq!(answer.send().await.unwrap().status(), 200, "{at}");
    }
    running.signal(Signal::TERM);
    assert_eq!(running.exit_code().await, Some(0));
}
