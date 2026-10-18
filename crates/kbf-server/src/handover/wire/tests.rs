//! The handover's lines.

use std::os::unix::ffi::OsStringExt as _;

use super::*;

fn addr(text: &str) -> SocketAddr {
    text.parse().expect("an address")
}

fn hello(instance: Option<Instance>, reapi: &str, state: PathBuf) -> Message {
    Message::Hello {
        version: PROTOCOL,
        instance,
        reapi: addr(reapi),
        state,
    }
}

/// Catches: a message whose line does not parse back to it (a successor and a holder
/// that disagree on what was said), a state path whose spaces, line breaks or non-UTF-8
/// bytes break the framing or are lost (the path is hex on the wire), and a reason that
/// carries a line break into a second, bogus message.
#[test]
fn every_message_is_one_line_that_parses_back() {
    let odd = PathBuf::from(OsString::from_vec(b"/var/lib/a b\n\xff/state.db".to_vec()));
    let messages = [
        hello(
            None,
            "127.0.0.1:1000",
            PathBuf::from("/var/lib/kbf/state.db"),
        ),
        hello(Some(Instance::A), "[::1]:3000", odd),
        hello(Some(Instance::B), "127.0.0.1:3010", PathBuf::from("/s")),
        Message::Offer { generation: 7 },
        Message::Offer { generation: 0 },
        Message::Serving,
        Message::Refused("no".to_owned()),
        Message::Failed("the store said no".to_owned()),
        Message::Ready,
        Message::Released,
    ];
    for message in messages {
        let line = message.line();
        assert!(!line.contains(['\n', '\r']), "{line:?}");
        assert_eq!(
            Message::parse(&format!("{line}\n")),
            Some(message),
            "{line}"
        );
    }
    let broken = Message::Refused("two\nlines\r".to_owned());
    assert_eq!(broken.line(), "refused two lines ");
    assert_eq!(
        Message::parse("failed\n"),
        Some(Message::Failed(String::new())),
        "a reason may be empty"
    );
}

/// Catches: a line that is not a message taken for one (a garbled hello would name the
/// wrong address, instance or none), for each way a hello or an offer can be wrong: a
/// missing field, a field that does not parse, an instance that is not a letter of the
/// pair, a state path that is not hex.
#[test]
fn a_line_that_is_not_a_message_is_none() {
    for line in [
        "",
        "launch",
        "hello",
        "hello x a 127.0.0.1:1 00",
        "hello 2 c 127.0.0.1:1 00",
        "hello 2 a 127.0.0.1 00",
        "hello 2 a nowhere 00",
        "hello 2 a 127.0.0.1:1",
        "hello 2 a 127.0.0.1:1 zz",
        "offer",
        "offer seven",
    ] {
        assert_eq!(Message::parse(line), None, "{line:?}");
    }
}

async fn pair() -> (Link, Link) {
    let (a, b) = UnixStream::pair().expect("a socket pair");
    (Link::new(a), Link::new(b))
}

/// Catches: a link that loses, reorders or merges lines, and the three ways a read ends
/// without a message: the other end closed (not a hang), a line that is not a message
/// (named in the error), and an I/O error.
#[tokio::test]
async fn a_link_carries_lines_and_says_how_a_read_ended() {
    let (mut a, mut b) = pair().await;
    a.say(&Message::Ready).await.expect("say");
    a.say(&Message::Released).await.expect("say");
    assert_eq!(b.hear().await.expect("hear"), Message::Ready);
    assert_eq!(b.hear().await.expect("hear"), Message::Released);

    a.stream
        .get_mut()
        .write_all(b"not a message\n")
        .await
        .expect("write");
    let malformed = b.hear().await.expect_err("malformed");
    assert!(
        matches!(&malformed, WireError::Malformed(line) if line == "not a message"),
        "{malformed}"
    );

    drop(a);
    let closed = b.hear().await.expect_err("closed");
    assert!(matches!(closed, WireError::Closed), "{closed}");
    assert_eq!(
        closed.to_string(),
        "the other process closed the control connection"
    );

    let io = WireError::from(io::Error::other("broken"));
    assert_eq!(io.to_string(), "control socket: broken");
}
