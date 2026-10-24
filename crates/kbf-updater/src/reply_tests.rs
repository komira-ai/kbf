//! The answer of an HTTP/1.1 call: a length, a chunked body, and what is refused.

use crate::ports::PortErrorKind;
use crate::reply::Reply;

fn parse(raw: &[u8]) -> Result<Reply, (PortErrorKind, String)> {
    Reply::parse(raw, "the API").map_err(|e| (e.kind(), e.to_string()))
}

/// A body with a length is read to that length and no further; a body with none is read to
/// the end.
#[test]
fn a_body_is_as_long_as_its_length_says() {
    let ok = parse(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}extra").unwrap();
    assert_eq!((ok.status, ok.body.as_slice()), (200, &b"{}"[..]));
    let all = parse(b"HTTP/1.1 404 Not Found\r\n\r\nbody").unwrap();
    assert_eq!((all.status, all.body.as_slice()), (404, &b"body"[..]));
}

/// A chunked body is joined from its chunks, whatever their sizes, with an extension and
/// trailers ignored.
#[test]
fn a_chunked_body_is_joined() {
    let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: Chunked\r\n\r\n\
                3\r\nabc\r\n0A;ext=1\r\n0123456789\r\n0\r\nX-Trailer: 1\r\n\r\n";
    let reply = parse(raw).unwrap();
    assert_eq!(reply.body, b"abc0123456789");
    let empty = parse(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n").unwrap();
    assert!(empty.body.is_empty());
}

/// A chunked body that is cut, mis-sized or badly framed is no answer, and a chunked answer
/// that also has a length, or another encoding, is refused: nothing is guessed at.
#[test]
fn a_bad_chunked_body_and_other_encodings_are_refused() {
    let head = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n";
    for body in [
        "",
        "3\r\nabc",
        "3\r\nabcXX0\r\n\r\n",
        "3\r\nab",
        "zz\r\nab\r\n",
        "\r\n",
        "+3\r\nabc\r\n0\r\n\r\n",
        "ffffffffffffffff\r\nabc",
        "5000000000000000000000\r\nabc",
        "3\r\nabc\r\n",
        "3",
    ] {
        let (kind, text) = parse(format!("{head}{body}").as_bytes()).unwrap_err();
        assert_eq!(kind, PortErrorKind::Unreachable, "{body:?}");
        assert!(
            text.contains("chunked answer that is cut or wrong"),
            "{text}"
        );
    }
    let (kind, text) = parse(
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Length: 5\r\n\r\n0\r\n\r\n",
    )
    .unwrap_err();
    assert_eq!(kind, PortErrorKind::Other);
    assert!(text.contains("with a length"), "{text}");
    let (kind, text) = parse(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip\r\n\r\nxx").unwrap_err();
    assert_eq!(kind, PortErrorKind::Other);
    assert!(text.contains("encoding"), "{text}");
}
