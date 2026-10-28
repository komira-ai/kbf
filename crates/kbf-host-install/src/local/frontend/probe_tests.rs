//! The probe of a frontend's operator API: a real connection to a server the test plays, and
//! the two questions asked through the port.

use std::cell::RefCell;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use super::probe::{Http, Probe, Reply, commit, ready};
use crate::pace::{fake::Fake, real_now};

/// A listener on loopback. The kernel picks the port from the range every connection on this
/// machine draws its source ports from, and a box that has thousands of connections in
/// `TIME_WAIT` can refuse a bind (`AddrInUse`) for a moment, so a refusal is tried again.
fn bind() -> (TcpListener, SocketAddr) {
    let mut tries = 0;
    loop {
        match TcpListener::bind("127.0.0.1:0") {
            Ok(listener) => {
                let addr = listener.local_addr().unwrap();
                return (listener, addr);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && tries < 100 => {
                tries += 1;
                thread::yield_now();
            }
            Err(e) => panic!("bind: {e}"),
        }
    }
}

/// A server that answers its connections in turn, one `answer` each, then closes each. The
/// requests it read. The listener is bound before this returns, so a client never dials a
/// port nobody holds, and one port serves every answer of a test.
fn serve_all(answers: Vec<Vec<u8>>) -> (SocketAddr, JoinHandle<Vec<String>>) {
    let (listener, addr) = bind();
    let handle = thread::spawn(move || {
        let mut requests = Vec::new();
        for answer in answers {
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
            requests.push(String::from_utf8(request).unwrap());
        }
        requests
    });
    (addr, handle)
}

/// `GET path` at `addr` for a server that is bound: a connection that the machine reset or
/// timed out before the server took it (the listener is bound, so only the machine's own
/// handling of a source port that is in `TIME_WAIT` does that) is dialled again, as the
/// program's own wait for readiness dials again. Anything else is the answer.
fn get(addr: SocketAddr, path: &str) -> Result<Reply, String> {
    let mut tries = 0;
    loop {
        let answer = http().get(addr, path);
        let transient = |why: &str| why.contains("os error 104") || why.contains("timed out");
        match &answer {
            Err(why) if transient(why) && tries < 20 => tries += 1,
            _ => return answer,
        }
    }
}

fn http() -> Http {
    Http {
        limit: Duration::from_secs(5),
        now: real_now(),
    }
}

/// The request is a plain HTTP/1.0 `GET` that asks for the connection to be closed, and the
/// status and the body come back as sent.
#[test]
fn a_get_is_sent_and_the_status_and_body_come_back() {
    let (addr, server) = serve_all(vec![
        b"HTTP/1.1 200 OK\r\ncontent-length: 13\r\n\r\n{\"ready\":true}".to_vec(),
        b"HTTP/1.0 503 Service Unavailable\r\n\r\n".to_vec(),
    ]);
    let reply = get(addr, "/readyz").unwrap();
    assert_eq!(
        reply,
        Reply {
            status: 200,
            body: "{\"ready\":true}".to_owned()
        }
    );
    assert_eq!(get(addr, "/readyz").unwrap().status, 503);
    assert_eq!(
        server.join().unwrap(),
        vec![format!("GET /readyz HTTP/1.0\r\nHost: {addr}\r\nConnection: close\r\n\r\n"); 2]
    );
}

/// An answer that is not an HTTP/1 response is an error, and so is a server that is not there.
#[test]
fn what_is_not_an_http_answer_is_an_error() {
    let bads: Vec<Vec<u8>> = [
        &b"hello\r\n\r\n"[..],
        b"HTTP/2 200\r\n\r\n",
        b"HTTP/1.1 OK\r\n\r\n",
        b"HTTP/1.1 200 OK\r\n",
        b"HTTP/1.1 200 OK\r\n\xff\r\n\r\n",
        b"",
    ]
    .iter()
    .map(|bad| bad.to_vec())
    .collect();
    let (addr, _) = serve_all(bads.clone());
    for bad in &bads {
        let why = get(addr, "/readyz").unwrap_err();
        assert!(why.contains("not an HTTP response"), "{bad:?}: {why}");
    }

    // Nothing listens on port 1 (a port that is not drawn for a source port or a test's
    // server), so the dial is refused; it is not a port a test freed a moment ago.
    let addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
    assert!(http().get(addr, "/readyz").unwrap_err().contains("/readyz"));
}

/// A call is given a limit for all of it, counted on the clock the probe was given. The limit
/// here is an hour and the clock jumps past it, so that nothing real is waited for: a call
/// whose time is up is an error before it reads again, with the server still silent, and one
/// whose answer keeps coming is cut at the limit, after as many reads as the time allowed.
#[test]
fn a_call_ends_at_its_limit_on_the_clock_it_was_given() {
    let hour = Duration::from_secs(3600);

    // A server that accepts and says nothing until the call has ended. The first reading is
    // the start of the call; the second, after the connection, is two hours later.
    let (listener, addr) = bind();
    let (release, released) = std::sync::mpsc::channel::<()>();
    let silent = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let _ = released.recv();
        drop(stream);
    });
    let clock = Fake::stepping(2 * hour);
    let silent_probe = Http {
        limit: hour,
        now: clock.now_fn(),
    };
    let why = silent_probe.get(addr, "/readyz").unwrap_err();
    assert!(why.contains("/readyz"), "{why}");
    assert_eq!(
        clock.reads(),
        2,
        "the start, and the one look at what was left"
    );
    release.send(()).unwrap();
    silent.join().unwrap();

    // One whose answer never ends (more than a read takes at a time) and has no end of head:
    // each look at the clock is twenty minutes later, so the fourth finds the hour up.
    let (listener, addr) = bind();
    let dripping = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let _ = stream.write_all(&[b'X'; 60 * 1024]);
    });
    let clock = Fake::stepping(hour / 3);
    let dripped = Http {
        limit: hour,
        now: clock.now_fn(),
    };
    let why = dripped.get(addr, "/readyz").unwrap_err();
    assert!(why.contains("/readyz"), "{why}");
    assert!(
        !why.contains("not an HTTP response"),
        "cut at the limit, not read to the end: {why}"
    );
    assert_eq!(
        clock.reads(),
        4,
        "the start, two reads, and the look that found the hour up"
    );
    dripping.join().unwrap();
}

/// An answer larger than the limit is cut there, and the head is still read.
#[test]
fn an_answer_is_read_up_to_its_limit() {
    let mut big = b"HTTP/1.1 200 OK\r\n\r\n".to_vec();
    big.extend(std::iter::repeat_n(b'a', 200 * 1024));
    let (addr, _) = serve_all(vec![big]);
    let reply = get(addr, "/healthz").unwrap();
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
