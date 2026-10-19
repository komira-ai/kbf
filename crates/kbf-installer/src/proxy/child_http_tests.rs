//! Tests of the loopback client against a fixture server on `127.0.0.1`: each test starts
//! a listener that takes one connection, reads the request, runs a script that plays the
//! child and hands the request bytes back, so a test sees both what the client sent and
//! what it made of the answer.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::child_http::{Client, Headers, HttpError, Limits, Response};

const OK_JSON: &[u8] =
    b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 11\r\n\r\n{\"ok\":true}";

/// Ten seconds: a call that is not about time never meets it.
fn limits() -> Limits {
    Limits {
        deadline: Duration::from_secs(10),
        max_body: 1024,
    }
}

fn within(deadline: Duration) -> Limits {
    Limits {
        deadline,
        max_body: 1024,
    }
}

struct Fixture {
    addr: SocketAddr,
    thread: JoinHandle<Vec<u8>>,
}

impl Fixture {
    /// Take one connection, read the request in full, run `script` on the connection and
    /// let it close.
    fn serve(script: impl FnOnce(&mut TcpStream) + Send + 'static) -> Fixture {
        Fixture::serve_raw(move |stream| {
            let request = read_request(stream);
            script(stream);
            request
        })
    }

    /// Take one connection and hand it to `script` unread.
    fn serve_raw(script: impl FnOnce(&mut TcpStream) -> Vec<u8> + Send + 'static) -> Fixture {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let thread = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            script(&mut stream)
        });
        Fixture { addr, thread }
    }

    /// Answer with `raw`, whole.
    fn answer(raw: Vec<u8>) -> Fixture {
        Fixture::serve(move |stream| {
            let _ = stream.write_all(&raw);
        })
    }

    fn client(&self, limits: Limits) -> Client {
        Client::new(self.addr, limits).unwrap()
    }

    /// What the client sent. Waits for the server's thread, so it also proves the script ended.
    fn request(self) -> Vec<u8> {
        self.thread.join().unwrap()
    }
}

/// The request on `stream`: its head, and the body its `Content-Length` announces.
fn read_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut got = Vec::new();
    let mut buf = [0u8; 512];
    loop {
        if let Some(end) = got.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&got[..end]).to_ascii_lowercase();
            let want = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length: "))
                .map_or(0, |n| n.parse::<usize>().unwrap());
            if got.len() >= end + 4 + want {
                return got;
            }
        }
        let n = stream.read(&mut buf).unwrap();
        if n == 0 {
            return got;
        }
        got.extend_from_slice(&buf[..n]);
    }
}

/// Block until the client closes its end.
fn wait_for_close(stream: &mut TcpStream) {
    let mut one = [0u8; 1];
    let _ = stream.read(&mut one);
}

/// `GET /x` against a server that answers with `raw`.
fn get_after(raw: &[u8]) -> Result<Response, HttpError> {
    let fixture = Fixture::answer(raw.to_vec());
    let reply = fixture.client(limits()).get("/x", &Headers::new());
    fixture.request();
    reply
}

fn headers(pairs: &[(&str, &str)]) -> Headers {
    pairs.iter().fold(Headers::new(), |headers, (name, value)| {
        headers.with(name, value.as_bytes()).unwrap()
    })
}

// ----- the request and the answer -----

#[test]
fn a_get_sends_the_headers_the_caller_named_and_returns_status_and_body() {
    // Catches: a request line, Host, Connection, header order or framing that drifts; a
    // Content-Length on a GET; an Origin header; a header the caller named that is dropped.
    let fixture = Fixture::answer(OK_JSON.to_vec());
    let addr = fixture.addr;
    let auth = headers(&[
        ("X-Kbf-Proxy-Auth", "s3cret"),
        ("Authorization", "Bearer tok"),
    ]);
    let reply = fixture
        .client(limits())
        .get("/v1/admin/role", &auth)
        .unwrap();
    assert_eq!(
        reply,
        Response {
            status: 200,
            body: b"{\"ok\":true}".to_vec()
        }
    );
    assert_eq!(
        String::from_utf8(fixture.request()).unwrap(),
        format!(
            "GET /v1/admin/role HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\
             X-Kbf-Proxy-Auth: s3cret\r\nAuthorization: Bearer tok\r\n\r\n"
        )
    );
}

#[test]
fn a_post_sends_its_length_and_body_after_the_headers() {
    // Catches: a body sent without Content-Length or cut short; the body ahead of the
    // blank line; the caller's Content-Type dropped.
    let fixture = Fixture::answer(b"HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\n\r\n".to_vec());
    let addr = fixture.addr;
    let json = headers(&[("Content-Type", "application/json")]);
    let reply = fixture
        .client(limits())
        .post("/v1/admin/takeover", &json, br#"{"expect":7}"#)
        .unwrap();
    assert_eq!(
        reply,
        Response {
            status: 202,
            body: Vec::new()
        }
    );
    assert_eq!(
        String::from_utf8(fixture.request()).unwrap(),
        format!(
            "POST /v1/admin/takeover HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\
             Content-Length: 12\r\nContent-Type: application/json\r\n\r\n{{\"expect\":7}}"
        )
    );
}

#[test]
fn json_and_and_put_the_content_type_after_the_callers_headers_in_order() {
    // Catches: a JSON type spelled other than a child's write gate reads it
    // (`application/json`); `and` dropping a header or putting them in another order.
    let fixture = Fixture::answer(b"HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\n\r\n".to_vec());
    let addr = fixture.addr;
    let both = headers(&[("X-Kbf-Proxy-Auth", "s3cret")]).and(&Headers::json());
    fixture.client(limits()).post("/x", &both, b"{}").unwrap();
    assert_eq!(
        String::from_utf8(fixture.request()).unwrap(),
        format!(
            "POST /x HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nContent-Length: 2\r\n\
             X-Kbf-Proxy-Auth: s3cret\r\nContent-Type: application/json\r\n\r\n{{}}"
        )
    );
}

#[test]
fn a_status_outside_2xx_comes_back_with_its_body_and_success_turns_it_into_an_error() {
    // Catches: a refusal's body lost (a 409 carries the role view); a 4xx returned as Ok
    // by `success`.
    let reply = get_after(b"HTTP/1.1 409 Conflict\r\ncontent-length: 4\r\n\r\nnope").unwrap();
    assert_eq!(
        reply,
        Response {
            status: 409,
            body: b"nope".to_vec()
        }
    );
    assert_eq!(
        reply.success().unwrap_err(),
        HttpError::Status {
            code: 409,
            body: b"nope".to_vec()
        }
    );
}

#[test]
fn success_is_exactly_the_2xx_range() {
    // Catches: a boundary moved by one at either end of 200..300.
    for status in [200, 204, 299] {
        let reply = Response {
            status,
            body: b"b".to_vec(),
        };
        assert_eq!(reply.success().unwrap(), b"b");
    }
    for status in [199, 300, 404, 503] {
        let reply = Response {
            status,
            body: b"b".to_vec(),
        };
        assert_eq!(
            reply.success().unwrap_err(),
            HttpError::Status {
                code: status,
                body: b"b".to_vec()
            }
        );
    }
}

#[test]
fn an_answer_in_pieces_is_put_together() {
    // Catches: a head or a body read as if one read held it.
    let fixture = Fixture::serve(|stream| {
        for piece in [
            &b"HTTP/1.1 200 OK\r\ncon"[..],
            b"tent-length: 2\r\n\r\no",
            b"k",
        ] {
            stream.write_all(piece).unwrap();
            thread::sleep(Duration::from_millis(40));
        }
    });
    let reply = fixture.client(limits()).get("/x", &Headers::new()).unwrap();
    assert_eq!(reply.body, b"ok");
    fixture.request();
}

#[test]
fn bytes_past_the_content_length_are_not_part_of_the_body() {
    // Catches: a body read to the end of the connection instead of to its length.
    let reply = get_after(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nokEXTRA").unwrap();
    assert_eq!(reply.body, b"ok");
}

#[test]
fn an_answer_with_no_body_by_status_needs_no_content_length() {
    // Catches: a 204 or 304 refused for lacking a length, or waiting for a body.
    for line in ["204 No Content", "304 Not Modified"] {
        let reply = get_after(format!("HTTP/1.1 {line}\r\n\r\n").as_bytes()).unwrap();
        assert_eq!(reply.body, b"");
        assert_eq!(reply.status, line[..3].parse::<u16>().unwrap());
    }
}

#[test]
fn a_repeated_content_length_with_one_value_is_read() {
    let reply =
        get_after(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\ncontent-length: 2\r\n\r\nok").unwrap();
    assert_eq!(reply.body, b"ok");
}

// ----- JSON -----

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Expect {
    expect: u64,
}

#[test]
fn json_goes_out_and_comes_back() {
    // Catches: a body not serialised as given; an answer not read as JSON.
    let fixture =
        Fixture::answer(b"HTTP/1.1 200 OK\r\ncontent-length: 12\r\n\r\n{\"expect\":8}".to_vec());
    let json = headers(&[("content-type", "application/json")]);
    let got: Expect = fixture
        .client(limits())
        .post_json("/t", &json, &Expect { expect: 7 })
        .unwrap();
    assert_eq!(got, Expect { expect: 8 });
    let request = String::from_utf8(fixture.request()).unwrap();
    assert!(request.ends_with("\r\n\r\n{\"expect\":7}"), "{request}");

    let fixture =
        Fixture::answer(b"HTTP/1.1 200 OK\r\ncontent-length: 12\r\n\r\n{\"expect\":9}".to_vec());
    let got: Expect = fixture
        .client(limits())
        .get_json("/t", &Headers::new())
        .unwrap();
    assert_eq!(got, Expect { expect: 9 });
    fixture.request();
}

#[test]
fn a_json_call_to_a_refusing_child_is_a_status_error_with_the_body() {
    // Catches: a 409's role view dropped on the JSON path, or its body parsed as the answer.
    let fixture = Fixture::answer(b"HTTP/1.1 409 Conflict\r\ncontent-length: 2\r\n\r\n{}".to_vec());
    let err = fixture
        .client(limits())
        .get_json::<Expect>("/t", &Headers::new())
        .unwrap_err();
    assert_eq!(
        err,
        HttpError::Status {
            code: 409,
            body: b"{}".to_vec()
        }
    );
    fixture.request();
}

#[test]
fn a_body_that_is_not_the_json_asked_for_is_a_json_error() {
    let fixture = Fixture::answer(b"HTTP/1.1 200 OK\r\ncontent-length: 4\r\n\r\nnope".to_vec());
    let err = fixture
        .client(limits())
        .get_json::<Expect>("/t", &Headers::new())
        .unwrap_err();
    assert!(matches!(err, HttpError::Json(_)), "{err:?}");
    fixture.request();
}

#[test]
fn a_body_json_cannot_hold_is_refused_before_anything_is_sent() {
    // A map keyed by a pair has no JSON form. The address has no listener, so a send
    // would be `Refused`.
    let client = Client::new(unused_port(), limits()).unwrap();
    let body: BTreeMap<(u8, u8), u8> = BTreeMap::from([((1, 2), 3)]);
    let err = client
        .post_json::<_, Expect>("/t", &Headers::new(), &body)
        .unwrap_err();
    assert!(matches!(err, HttpError::Json(_)), "{err:?}");
}

// ----- where it may connect -----

#[test]
fn only_a_loopback_address_gets_a_client() {
    // Catches: the loopback check removed or narrowed to 127.0.0.1; a private, unspecified,
    // link-local or documentation address, or one mapped into IPv6, let through.
    let mapped = |a: Ipv4Addr| SocketAddr::from((a.to_ipv6_mapped(), 80));
    let allowed = [
        "127.0.0.1:80".parse().unwrap(),
        SocketAddr::from((Ipv4Addr::new(127, 1, 2, 3), 80)),
        "[::1]:80".parse().unwrap(),
        mapped(Ipv4Addr::LOCALHOST),
    ];
    for addr in allowed {
        assert!(Client::new(addr, limits()).is_ok(), "{addr}");
    }
    let refused = [
        SocketAddr::from((Ipv4Addr::new(10, 1, 2, 3), 80)),
        SocketAddr::from((Ipv4Addr::new(172, 16, 0, 9), 80)),
        SocketAddr::from((Ipv4Addr::new(192, 168, 1, 1), 80)),
        "192.0.2.1:80".parse().unwrap(),
        "0.0.0.0:80".parse().unwrap(),
        "[::]:80".parse().unwrap(),
        "[fe80::1]:80".parse().unwrap(),
        "[2001:db8::1]:80".parse().unwrap(),
        mapped(Ipv4Addr::new(10, 1, 2, 3)),
    ];
    for addr in refused {
        assert_eq!(
            Client::new(addr, limits()).unwrap_err(),
            HttpError::NotLoopback(addr)
        );
    }
}

/// A loopback address that nothing listens on.
fn unused_port() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap()
}

#[test]
fn a_connection_nobody_accepts_is_refused() {
    let client = Client::new(unused_port(), limits()).unwrap();
    assert_eq!(
        client.get("/x", &Headers::new()).unwrap_err(),
        HttpError::Refused
    );
}

#[test]
fn a_connection_the_server_resets_is_an_io_error() {
    // The server reads one byte of the request and closes with the rest unread, which
    // makes the kernel send a reset.
    let fixture = Fixture::serve_raw(|stream| {
        let mut one = [0u8; 1];
        stream.read_exact(&mut one).unwrap();
        Vec::new()
    });
    let err = fixture
        .client(limits())
        .post("/x", &Headers::new(), &[7u8; 256])
        .unwrap_err();
    assert!(matches!(err, HttpError::Io(_)), "{err:?}");
    fixture.request();
}

#[test]
fn a_connection_reset_while_the_request_is_written_is_an_io_error() {
    // A body far over the socket buffers, to a server that resets at once: the error
    // comes from the write, before any answer is read.
    let fixture = Fixture::serve_raw(|stream| {
        let mut one = [0u8; 1];
        stream.read_exact(&mut one).unwrap();
        Vec::new()
    });
    let err = fixture
        .client(limits())
        .post("/x", &Headers::new(), &vec![7u8; 32 << 20])
        .unwrap_err();
    assert!(matches!(err, HttpError::Io(_)), "{err:?}");
    fixture.request();
}

#[test]
fn a_path_that_could_split_the_request_is_refused_before_connecting() {
    // Catches: a path with a space or line break written into the request line; a path
    // that is not absolute. The address has no listener, so a send would be `Refused`.
    let client = Client::new(unused_port(), limits()).unwrap();
    for path in ["v1/x", "", "/a b", "/a\r\nOrigin: x", "/a\nb", "/caf\u{e9}"] {
        assert!(
            matches!(
                client.get(path, &Headers::new()),
                Err(HttpError::BadRequest(_))
            ),
            "{path:?}"
        );
    }
}

// ----- the headers the caller may name -----

#[test]
fn a_header_cannot_name_what_the_client_owns() {
    // Catches: Origin, or a framing header, accepted from the caller, in any case.
    for name in [
        "Host",
        "content-length",
        "TRANSFER-ENCODING",
        "Connection",
        "Origin",
        "origin",
    ] {
        let err = Headers::new().with(name, b"x").unwrap_err();
        assert!(matches!(err, HttpError::BadRequest(_)), "{name}");
    }
}

#[test]
fn a_header_cannot_have_a_name_or_value_that_breaks_the_line() {
    // Catches: a token with a line break that adds a header of its own; a name that is
    // not a token.
    for name in ["", "a b", "a:b", "caf\u{e9}", "x\r\ny"] {
        let err = Headers::new().with(name, b"x").unwrap_err();
        assert!(matches!(err, HttpError::BadRequest(_)), "{name:?}");
    }
    for value in [
        &b"a\r\nOrigin: x"[..],
        b"a\nb",
        b"a\rb",
        b"a\0b",
        b"a\x7fb",
        "caf\u{e9}".as_bytes(),
    ] {
        let err = Headers::new().with("x-a", value).unwrap_err();
        assert!(matches!(err, HttpError::BadRequest(_)), "{value:?}");
    }
    assert!(Headers::new().with("x-a", b"").is_ok());
    assert!(Headers::new().with("!#$%&'*+-.^_`|~", b"a\tb c~").is_ok());
}

#[test]
fn debug_shows_no_header_value() {
    // Catches: a bearer token or the proxy secret printed by a `{:?}` of a request.
    let secret = headers(&[("authorization", "Bearer hunter2")]);
    let shown = format!("{secret:?}");
    assert!(
        !shown.contains("hunter2") && !shown.contains("Bearer"),
        "{shown}"
    );
}

// ----- the deadline -----

#[test]
fn a_server_that_never_answers_ends_the_call_at_the_deadline() {
    // Catches: a call that waits for ever on a child that took the request and went quiet.
    let fixture = Fixture::serve(wait_for_close);
    let started = Instant::now();
    let err = fixture
        .client(within(Duration::from_millis(200)))
        .get("/x", &Headers::new())
        .unwrap_err();
    assert_eq!(err, HttpError::Timeout);
    assert!(started.elapsed() < Duration::from_secs(5));
    fixture.request();
}

#[test]
fn a_peer_that_stops_reading_ends_the_call_at_the_deadline() {
    // Catches: no deadline on the write. The server takes the connection and never reads
    // it; a 32 MiB body is far over what the socket buffers of both ends hold, so the
    // write blocks for good and only a write timeout ends the call. The call runs on a
    // thread of its own, so a client without that timeout fails this test at the wait
    // below and does not hang it.
    let (release, held) = mpsc::channel::<()>();
    let fixture = Fixture::serve_raw(move |_stream| {
        let _ = held.recv();
        Vec::new()
    });
    let client = fixture.client(within(Duration::from_millis(300)));
    let (done, outcome) = mpsc::channel();
    let caller = thread::spawn(move || {
        let started = Instant::now();
        let reply = client.post("/x", &Headers::new(), &vec![7u8; 32 << 20]);
        let _ = done.send((reply, started.elapsed()));
    });
    let outcome = outcome.recv_timeout(Duration::from_secs(10));
    // Letting go closes the connection with the body unread, which frees a stuck writer.
    release.send(()).unwrap();
    caller.join().unwrap();
    fixture.request();
    let (reply, elapsed) =
        outcome.expect("the call was still writing 10 s after its 300 ms deadline");
    assert_eq!(reply.unwrap_err(), HttpError::Timeout);
    assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
}

/// Write `bytes` one at a time, `gap` apart, until the client closes.
fn trickle(stream: &mut TcpStream, bytes: &[u8], gap: Duration) {
    stream.set_nodelay(true).unwrap();
    for byte in bytes {
        if stream.write_all(&[*byte]).is_err() {
            return;
        }
        thread::sleep(gap);
    }
}

#[test]
fn a_body_that_trickles_is_cut_at_the_total_deadline() {
    // Catches: a deadline applied to each read. Each byte arrives 40 ms after the last, well
    // inside a 500 ms read timeout, but the 60 bytes need 2.4 s: only a deadline on the
    // whole call stops it.
    let fixture = Fixture::serve(|stream| {
        stream
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 60\r\n\r\n")
            .unwrap();
        trickle(stream, &[b'a'; 60], Duration::from_millis(40));
    });
    let err = fixture
        .client(within(Duration::from_millis(500)))
        .get("/x", &Headers::new())
        .unwrap_err();
    assert_eq!(err, HttpError::Timeout);
    fixture.request();
}

#[test]
fn a_head_that_trickles_is_cut_at_the_total_deadline() {
    // Catches: the same, in the status line and headers.
    let fixture = Fixture::serve(|stream| {
        trickle(
            stream,
            b"HTTP/1.1 200 OK\r\nx-pad: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\r\n\r\n",
            Duration::from_millis(40),
        );
    });
    let err = fixture
        .client(within(Duration::from_millis(500)))
        .get("/x", &Headers::new())
        .unwrap_err();
    assert_eq!(err, HttpError::Timeout);
    fixture.request();
}

#[test]
fn a_zero_deadline_times_out_without_connecting() {
    // Catches: a zero timeout handed to the socket, which means "wait for ever", and a
    // connection made on a budget that is gone.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let client = Client::new(listener.local_addr().unwrap(), within(Duration::ZERO)).unwrap();
    assert_eq!(
        client.get("/x", &Headers::new()).unwrap_err(),
        HttpError::Timeout
    );
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
}

#[test]
fn an_unbounded_deadline_is_cut_not_a_panic() {
    // Catches: `Instant + Duration::MAX` overflowing.
    let fixture = Fixture::answer(OK_JSON.to_vec());
    let reply = fixture
        .client(within(Duration::MAX))
        .get("/x", &Headers::new())
        .unwrap();
    assert_eq!(reply.status, 200);
    fixture.request();
}

// ----- the size limit -----

#[test]
fn a_body_over_the_limit_is_refused_from_its_head() {
    // Catches: the size limit off, or applied after the body was read. The server sends
    // no body at all, so a client that waited for one would time out instead.
    let fixture = Fixture::serve(|stream| {
        stream
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 1025\r\n\r\n")
            .unwrap();
        wait_for_close(stream);
    });
    let err = fixture
        .client(limits())
        .get("/x", &Headers::new())
        .unwrap_err();
    assert_eq!(err, HttpError::TooLarge { limit: 1024 });
    fixture.request();
}

#[test]
fn a_body_of_exactly_the_limit_is_read() {
    // Catches: the limit off by one.
    let mut raw = b"HTTP/1.1 200 OK\r\ncontent-length: 1024\r\n\r\n".to_vec();
    raw.extend_from_slice(&[b'x'; 1024]);
    let reply = get_after(&raw).unwrap();
    assert_eq!(reply.body, vec![b'x'; 1024]);
}

// ----- answers the client does not accept -----

#[test]
fn an_answer_that_is_not_http_1_1_with_a_length_is_malformed() {
    // Catches: each rule that keeps a body from being misframed (and a body size from
    // being misread) going missing, one case each.
    let many_headers: String = (0..65).map(|i| format!("x-{i}: 1\r\n")).collect();
    let long_header = format!("x-long: {}\r\n\r\n", "a".repeat(20_000));
    let cases: Vec<(&str, Vec<u8>, &str)> = vec![
        (
            "not http",
            b"this is not http\r\n\r\n".to_vec(),
            "not a status line and headers",
        ),
        (
            "too many headers",
            format!("HTTP/1.1 200 OK\r\ncontent-length: 0\r\n{many_headers}\r\n").into_bytes(),
            "not a status line and headers",
        ),
        (
            "http 1.0",
            b"HTTP/1.0 200 OK\r\ncontent-length: 0\r\n\r\n".to_vec(),
            "not HTTP/1.1",
        ),
        (
            "interim status",
            b"HTTP/1.1 100 Continue\r\ncontent-length: 0\r\n\r\n".to_vec(),
            "no final status",
        ),
        (
            "status past 599",
            b"HTTP/1.1 600 Odd\r\ncontent-length: 0\r\n\r\n".to_vec(),
            "no final status",
        ),
        (
            "no length",
            b"HTTP/1.1 200 OK\r\n\r\nbody".to_vec(),
            "the answer has no Content-Length",
        ),
        (
            "empty length",
            b"HTTP/1.1 200 OK\r\ncontent-length:\r\n\r\n".to_vec(),
            "Content-Length is not a number",
        ),
        (
            "signed length",
            b"HTTP/1.1 200 OK\r\ncontent-length: +2\r\n\r\nok".to_vec(),
            "Content-Length is not a number",
        ),
        (
            "length with a word",
            b"HTTP/1.1 200 OK\r\ncontent-length: 2 bytes\r\n\r\nok".to_vec(),
            "Content-Length is not a number",
        ),
        (
            "length overflow",
            b"HTTP/1.1 200 OK\r\ncontent-length: 99999999999999999999999\r\n\r\n".to_vec(),
            "Content-Length is not a number",
        ),
        (
            "two lengths",
            b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\ncontent-length: 3\r\n\r\nok".to_vec(),
            "Content-Length is given twice, differently",
        ),
        (
            "closed at once",
            Vec::new(),
            "the connection closed before the headers ended",
        ),
        (
            "closed inside the headers",
            b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n".to_vec(),
            "the connection closed before the headers ended",
        ),
        (
            "headers too long",
            [b"HTTP/1.1 200 OK\r\n".as_slice(), long_header.as_bytes()].concat(),
            "the headers are too long",
        ),
        (
            "closed inside the body",
            b"HTTP/1.1 200 OK\r\ncontent-length: 10\r\n\r\nabc".to_vec(),
            "the connection closed inside the body",
        ),
    ];
    for (name, raw, reason) in cases {
        assert_eq!(
            get_after(&raw).unwrap_err(),
            HttpError::Malformed(reason),
            "{name}"
        );
    }
}

#[test]
fn a_transfer_encoding_is_refused_not_misread() {
    // Catches: a chunked body read as if its chunk framing were the body.
    for raw in [
        &b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n2\r\nok\r\n0\r\n\r\n"[..],
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip\r\ncontent-length: 2\r\n\r\nok",
    ] {
        let err = get_after(raw).unwrap_err();
        assert!(matches!(err, HttpError::Unsupported(_)), "{err:?}");
    }
}
