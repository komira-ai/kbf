//! The operator API client, against a one-answer server on loopback that keeps the request.

use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::thread::JoinHandle;

use super::*;

/// A server that answers the next request with `answer` (status line, headers and body as
/// given) and returns the request it read, whole (the head and the body its length names).
pub(crate) fn serve(answer: &'static str) -> (u16, JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut text = Vec::new();
        let mut chunk = [0_u8; 1024];
        loop {
            let n = stream.read(&mut chunk).expect("read");
            text.extend_from_slice(&chunk[..n]);
            let seen = String::from_utf8_lossy(&text).into_owned();
            if let Some((head, body)) = seen.split_once("\r\n\r\n") {
                let want = head
                    .lines()
                    .find_map(|l| l.strip_prefix("Content-Length: "))
                    .map_or(0, |n| n.parse::<usize>().expect("a length"));
                if body.len() >= want {
                    break;
                }
            }
            assert!(n > 0, "the client closed before the request was whole");
        }
        stream.write_all(answer.as_bytes()).expect("answer");
        String::from_utf8(text).expect("text")
    });
    (port, handle)
}

#[test]
fn a_post_carries_the_token_the_body_and_its_length_and_no_origin() {
    // Catches: a POST the API refuses for what it lacks (the bearer token, the JSON content
    // type, the length of the body) or for what a browser's request has (an `Origin`), and an
    // answer whose status and body are not both returned. Mutants: the `Authorization` line
    // dropped; `Content-Length` of the wrong size; the body dropped from the answer.
    let (port, server) = serve("HTTP/1.1 409 Conflict\r\nContent-Length: 6\r\n\r\nnot up");
    let got = post(port, "tok-of-32", "/v1/members/a", r#"{"state":"x"}"#);
    assert_eq!(got, Some((409, "not up".to_owned())));
    let request = server.join().expect("server");
    assert!(request.starts_with("POST /v1/members/a HTTP/1.1\r\n"));
    assert!(request.contains("\r\nAuthorization: Bearer tok-of-32\r\n"));
    assert!(request.contains("\r\nContent-Type: application/json\r\n"));
    assert!(request.contains("\r\nContent-Length: 13\r\n"));
    assert!(request.ends_with("\r\n\r\n{\"state\":\"x\"}"));
    assert!(!request.to_lowercase().contains("origin"));
}

#[test]
fn a_get_returns_the_status_and_the_body() {
    // Catches: a GET that sends a body or a token, or an answer parsed at the wrong line.
    let (port, server) = serve("HTTP/1.1 200 OK\r\n\r\nhello");
    assert_eq!(get(port, "/x"), Some((200, "hello".to_owned())));
    let request = server.join().expect("server");
    assert_eq!(
        request,
        "GET /x HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\n\r\n"
    );
}

#[test]
fn nothing_listening_and_a_malformed_answer_are_none() {
    // Catches: a wait that takes silence or garbage for an answer (a panic, or a code 0).
    let closed = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = closed.local_addr().expect("addr").port();
    drop(closed);
    assert_eq!(get(port, "/x"), None);
    let (port, server) = serve("not http at all");
    assert_eq!(get(port, "/x"), None);
    server.join().expect("server");
    let (port, server) = serve("HTTP/1.1 abc\r\n\r\nbody");
    assert_eq!(get(port, "/x"), None);
    server.join().expect("server");
}

#[test]
fn the_members_are_read_with_their_id_address_and_state() {
    // Catches: a members reader that drops a member, reads the wrong field, or answers for
    // an error page or a member with no state (a test would then pick a member by a guess).
    let json = r#"{"generation":3,"members":[{"id":7,"addr":"192.0.2.2:9","state":"active","x":1},{"id":8,"addr":"192.0.2.3:9","state":"planned_down"}]}"#;
    let answer: &'static str = Box::leak(
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{json}",
            json.len()
        )
        .into_boxed_str(),
    );
    let (port, server) = serve(answer);
    let want = vec![
        Member {
            id: 7,
            addr: "192.0.2.2:9".to_owned(),
            state: "active".to_owned(),
        },
        Member {
            id: 8,
            addr: "192.0.2.3:9".to_owned(),
            state: "planned_down".to_owned(),
        },
    ];
    assert_eq!(members(port), Some(want));
    server.join().expect("server");

    let (port, server) = serve("HTTP/1.1 503 Unavailable\r\n\r\n{\"members\":[]}");
    assert_eq!(members(port), None);
    server.join().expect("server");
    let (port, server) = serve("HTTP/1.1 200 OK\r\n\r\n{\"members\":[{\"id\":7,\"addr\":\"x\"}]}");
    assert_eq!(members(port), None);
    server.join().expect("server");
}
