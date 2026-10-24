//! The probe of a frontend's operator API: a real connection to a server the test plays, and
//! the two questions asked through the port.

use std::cell::RefCell;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use super::probe::{Http, Probe, Reply, commit, ready};

/// A server that answers one connection with `answer`, then closes. The request it read.
fn serve(answer: Vec<u8>) -> (SocketAddr, JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = Vec::new();
        let mut byte = [0_u8; 1];
        while !request.ends_with(b"\r\n\r\n") {
            if stream.read(&mut byte).unwrap() == 0 {
                break;
            }
            request.push(byte[0]);
        }
        let _ = stream.write_all(&answer);
        String::from_utf8(request).unwrap()
    });
    (addr, handle)
}

fn http() -> Http {
    Http {
        limit: Duration::from_secs(5),
    }
}

/// The request is a plain HTTP/1.0 `GET` that asks for the connection to be closed, and the
/// status and the body come back as sent.
#[test]
fn a_get_is_sent_and_the_status_and_body_come_back() {
    let (addr, server) =
        serve(b"HTTP/1.1 200 OK\r\ncontent-length: 13\r\n\r\n{\"ready\":true}".to_vec());
    let reply = http().get(addr, "/readyz").unwrap();
    assert_eq!(
        reply,
        Reply {
            status: 200,
            body: "{\"ready\":true}".to_owned()
        }
    );
    assert_eq!(
        server.join().unwrap(),
        format!("GET /readyz HTTP/1.0\r\nHost: {addr}\r\nConnection: close\r\n\r\n")
    );

    let (addr, _) = serve(b"HTTP/1.0 503 Service Unavailable\r\n\r\n".to_vec());
    assert_eq!(http().get(addr, "/readyz").unwrap().status, 503);
}

/// An answer that is not an HTTP/1 response is an error, and so is a server that is not there
/// or does not finish in time.
#[test]
fn what_is_not_an_http_answer_is_an_error() {
    for bad in [
        &b"hello\r\n\r\n"[..],
        b"HTTP/2 200\r\n\r\n",
        b"HTTP/1.1 OK\r\n\r\n",
        b"HTTP/1.1 200 OK\r\n",
        b"HTTP/1.1 200 OK\r\n\xff\r\n\r\n",
        b"",
    ] {
        let (addr, _) = serve(bad.to_vec());
        let why = http().get(addr, "/readyz").unwrap_err();
        assert!(why.contains("not an HTTP response"), "{bad:?}: {why}");
    }

    let gone = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = gone.local_addr().unwrap();
    drop(gone);
    assert!(http().get(addr, "/readyz").unwrap_err().contains("/readyz"));

    // A server that accepts and says nothing: the call ends at its limit.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let silent = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        thread::sleep(Duration::from_millis(600));
        drop(stream);
    });
    let quick = Http {
        limit: Duration::from_millis(150),
    };
    assert!(quick.get(addr, "/readyz").is_err());
    silent.join().unwrap();

    // One that drips its answer for longer than the limit.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let dripping = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        for _ in 0..30 {
            if stream.write_all(b"X").is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
    });
    let why = quick.get(addr, "/readyz").unwrap_err();
    assert!(why.contains("/readyz"), "{why}");
    dripping.join().unwrap();
}

/// An answer larger than the limit is cut there, and the head is still read.
#[test]
fn an_answer_is_read_up_to_its_limit() {
    let mut big = b"HTTP/1.1 200 OK\r\n\r\n".to_vec();
    big.extend(std::iter::repeat_n(b'a', 200 * 1024));
    let (addr, _) = serve(big);
    let reply = http().get(addr, "/healthz").unwrap();
    assert_eq!(reply.status, 200);
    assert!(reply.body.len() < 100 * 1024);
}

/// A probe played by a list of answers.
struct Replies(RefCell<Vec<Result<Reply, String>>>);

impl Probe for Replies {
    fn get(&self, _addr: SocketAddr, _path: &str) -> Result<Reply, String> {
        self.0.borrow_mut().remove(0)
    }
}

fn reply(status: u16, body: &str) -> Result<Reply, String> {
    Ok(Reply {
        status,
        body: body.to_owned(),
    })
}

/// Ready is `/readyz` answering 200, and nothing else.
#[test]
fn ready_is_a_200_from_readyz() {
    let addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
    let asked = |answer| ready(&Replies(RefCell::new(vec![answer])), addr);
    assert!(asked(reply(200, "")));
    assert!(!asked(reply(503, "")));
    assert!(!asked(Err("refused".to_owned())));
}

/// The commit is the one `/healthz` reports with a 200.
#[test]
fn the_commit_is_what_healthz_reports() {
    let addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
    let asked = |answer| commit(&Replies(RefCell::new(vec![answer])), addr);
    assert_eq!(
        asked(reply(
            200,
            "{\"status\":\"alive\",\"commit\":\"0123456789ab\"}"
        ))
        .as_deref(),
        Some("0123456789ab")
    );
    assert_eq!(asked(reply(200, "{\"status\":\"alive\"}")), None);
    assert_eq!(asked(reply(200, "{\"commit\":7}")), None);
    assert_eq!(asked(reply(200, "not json")), None);
    assert_eq!(asked(reply(503, "{\"commit\":\"0123456789ab\"}")), None);
    assert_eq!(asked(Err("refused".to_owned())), None);
}
