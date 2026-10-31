//! The client against a peer it controls: a socket pair for the bytes. The path to a real
//! socket is tried against the real server, in the tests of `kbf-step-runner`.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use serde_json::{Value, json};

use super::*;

const OK: &str = "{\"ok\":true,\"error\":null}\n";

fn refusal_line(token: &str) -> String {
    format!("{{\"ok\":false,\"error\":\"{token}\"}}\n")
}

/// A client stream whose peer has already said `said`; the peer is returned with it.
fn peer_that_said(said: &[u8]) -> (UnixStream, UnixStream) {
    let (client, mut peer) = UnixStream::pair().unwrap();
    peer.write_all(said).unwrap();
    (client, peer)
}

/// The line the client sent: what the peer reads up to the newline.
fn sent_line(peer: &UnixStream) -> String {
    let mut line = String::new();
    BufReader::new(peer).read_line(&mut line).unwrap();
    line
}

fn refused(said: &str) -> Refusal {
    let (client, _peer) = peer_that_said(said.as_bytes());
    match register_on(client, "svc", "1.0.0", None) {
        Err(Error::Refused(refusal)) => refusal,
        other => panic!("{said:?}: {other:?}"),
    }
}

/// The registration is one JSON line of name, version and health, the health being null for a
/// service with no listener.
#[test]
fn the_registration_is_one_json_line() {
    let (client, peer) = peer_that_said(OK.as_bytes());
    let _held = register_on(client, "svc", "1.2.3", None).unwrap();
    let line = sent_line(&peer);
    assert!(line.ends_with('\n'));
    let sent: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(
        sent,
        json!({"name": "svc", "version": "1.2.3", "health": null})
    );
}

/// A health address is sent as text with its path, an IPv6 loopback in brackets.
#[test]
fn the_health_address_is_sent_with_its_path() {
    for addr in ["127.0.0.1:9301", "[::1]:9302"] {
        let (client, peer) = peer_that_said(OK.as_bytes());
        let health = Health::new(addr.parse().unwrap(), "/readyz");
        let _held = register_on(client, "svc", "1.0.0", Some(&health)).unwrap();
        let sent: Value = serde_json::from_str(&sent_line(&peer)).unwrap();
        assert_eq!(sent["health"], json!({"addr": addr, "path": "/readyz"}));
    }
}

/// A name or version with a quote, a newline or a non-ASCII character is escaped, so the line
/// stays one line and reads back as it was.
#[test]
fn a_name_with_a_newline_stays_one_line() {
    let (client, peer) = peer_that_said(OK.as_bytes());
    let _held = register_on(client, "a\"b\nc\u{e9}", "v\n2", None).unwrap();
    let line = sent_line(&peer);
    assert_eq!(line.matches('\n').count(), 1, "{line:?}");
    let sent: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(sent["name"], "a\"b\nc\u{e9}");
    assert_eq!(sent["version"], "v\n2");
}

/// The updater reads the connection as the liveness: while the registration is held the peer
/// sees an open connection with nothing more sent, and when it is dropped the peer sees it end.
#[test]
fn dropping_the_registration_closes_the_connection() {
    let (client, mut peer) = peer_that_said(OK.as_bytes());
    let held = register_on(client, "svc", "1.0.0", None).unwrap();
    let _ = sent_line(&peer);
    peer.set_nonblocking(true).unwrap();
    let mut byte = [0u8; 1];
    let open = peer.read(&mut byte).unwrap_err();
    assert_eq!(open.kind(), std::io::ErrorKind::WouldBlock);
    drop(held);
    assert_eq!(peer.read(&mut byte).unwrap(), 0);
}

/// Each reason the updater sends is its own variant, and the token of a variant is the one it
/// is read from.
#[test]
fn each_refusal_is_its_own_variant() {
    let table = [
        ("malformed", Refusal::Malformed),
        ("unknown_service", Refusal::UnknownService),
        ("wrong_user", Refusal::WrongUser),
        ("health_address", Refusal::HealthAddress),
        ("health_port", Refusal::HealthPort),
        ("busy", Refusal::Busy),
        ("internal", Refusal::Internal),
    ];
    for (token, want) in table {
        assert_eq!(refused(&refusal_line(token)), want, "{token}");
        assert_eq!(want.token(), token);
    }
}

/// A reason a newer updater sends is kept as it came, not read as one of ours.
#[test]
fn an_unknown_reason_is_kept() {
    let got = refused(&refusal_line("quota"));
    assert_eq!(got, Refusal::Other("quota".to_owned()));
    assert_eq!(got.token(), "quota");
    assert!(got.to_string().contains("quota"));
}

/// Every refusal has a message that names its cause, and the error that carries it shows it.
#[test]
fn a_refusal_reads_as_a_sentence() {
    let err = Error::from(Refusal::WrongUser);
    assert!(err.to_string().contains("another user"), "{err}");
    assert!(Refusal::Busy.to_string().contains("connections"));
    assert!(Refusal::Malformed.to_string().contains("size limit"));
    assert!(Refusal::UnknownService.to_string().contains("declaration"));
    assert!(Refusal::HealthAddress.to_string().contains("loopback"));
    assert!(Refusal::HealthPort.to_string().contains("port"));
    assert!(Refusal::Internal.to_string().contains("keep"));
}

/// An answer that is not one of the two the updater sends is an error that shows it, and is
/// never read as success.
#[test]
fn an_answer_that_is_not_understood_is_not_success() {
    let answers = [
        "{\"ok\":true,\"error\":\"busy\"}\n",
        "{\"ok\":false,\"error\":null}\n",
        "{\"ok\":false}\n",
        "{\"error\":null}\n",
        "{\"ok\":\"yes\",\"error\":null}\n",
        "[true]\n",
        "not json\n",
    ];
    for answer in answers {
        let (client, _peer) = peer_that_said(answer.as_bytes());
        let got = register_on(client, "svc", "1.0.0", None);
        match got {
            Err(Error::BadAnswer(text)) => assert_eq!(text, answer),
            other => panic!("{answer:?}: {other:?}"),
        }
    }
}

/// An answer longer than the limit with no end is not read to the end of memory.
#[test]
fn an_endless_answer_is_cut_off() {
    let (client, _peer) = peer_that_said(&vec![b'x'; 2 * MAX_ANSWER as usize]);
    match register_on(client, "svc", "1.0.0", None) {
        Err(Error::BadAnswer(text)) => assert_eq!(text.len(), MAX_ANSWER as usize),
        other => panic!("{other:?}"),
    }
}

/// An answer that is not text is an I/O error, not a panic.
#[test]
fn an_answer_that_is_not_text_is_an_error() {
    let (client, _peer) = peer_that_said(&[0xff, 0xfe, b'\n']);
    assert!(matches!(
        register_on(client, "svc", "1.0.0", None),
        Err(Error::Io(_))
    ));
}

/// A peer that hangs up without answering is `NoAnswer` when the line was sent, and the write
/// error when it was not.
#[test]
fn a_peer_that_hangs_up_is_told_apart_by_what_was_sent() {
    // The peer reads the line and closes without a word.
    let (client, peer) = UnixStream::pair().unwrap();
    let hang_up = std::thread::spawn(move || sent_line(&peer));
    let got = register_on(client, "svc", "1.0.0", None);
    hang_up.join().unwrap();
    assert!(matches!(got, Err(Error::NoAnswer)), "{got:?}");

    // The peer is gone before the line is written.
    let (client, peer) = UnixStream::pair().unwrap();
    drop(peer);
    let got = register_on(client, "svc", "1.0.0", None);
    assert!(matches!(got, Err(Error::Io(_))), "{got:?}");
}

/// An updater that answers `busy` and closes without reading the line is read for its answer
/// even though the write found the connection gone.
#[test]
fn the_answer_of_a_peer_that_closed_is_still_read() {
    let (client, peer) = peer_that_said(refusal_line("busy").as_bytes());
    drop(peer);
    assert!(matches!(
        register_on(client, "svc", "1.0.0", None),
        Err(Error::Refused(Refusal::Busy))
    ));
}

/// A registration over the updater's limit is refused here, before anything is sent, and one of
/// exactly the limit goes.
#[test]
fn a_registration_over_the_limit_is_not_sent() {
    // `{"health":null,"name":"svc","version":""}` and its newline.
    let overhead = r#"{"health":null,"name":"svc","version":""}"#.len() + 1;
    let (client, peer) = peer_that_said(OK.as_bytes());
    let version = "9".repeat(MAX_LINE - overhead);
    let _held = register_on(client, "svc", &version, None).unwrap();
    assert_eq!(sent_line(&peer).len(), MAX_LINE);

    let (client, peer) = peer_that_said(OK.as_bytes());
    let version = "9".repeat(MAX_LINE - overhead + 1);
    match register_on(client, "svc", &version, None) {
        Err(Error::TooLong { len }) => assert_eq!(len, MAX_LINE + 1),
        other => panic!("{other:?}"),
    }
    // The client was dropped without a byte sent.
    let mut rest = Vec::new();
    (&peer).read_to_end(&mut rest).unwrap();
    assert!(rest.is_empty());
}

/// A socket path nothing listens on is a connect error that names the path.
#[test]
fn a_missing_socket_names_its_path() {
    let sock = Path::new("/nonexistent-update-register/none");
    match register(sock, "svc", "1.0.0", None) {
        Err(Error::Connect { path, source }) => {
            assert_eq!(path, sock);
            assert_eq!(source.kind(), std::io::ErrorKind::NotFound);
            let shown = Error::Connect { path, source }.to_string();
            assert!(shown.contains("nonexistent-update-register"), "{shown}");
        }
        other => panic!("{other:?}"),
    }
}

/// The other errors read as sentences too.
#[test]
fn the_errors_show_what_they_hold() {
    assert!(Error::TooLong { len: 5000 }.to_string().contains("5000"));
    assert!(Error::NoAnswer.to_string().contains("without answering"));
    assert!(Error::BadAnswer("zz".into()).to_string().contains("zz"));
    let io = Error::from(std::io::Error::other("boom"));
    assert!(io.to_string().contains("boom"));
}
