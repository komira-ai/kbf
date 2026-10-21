//! The host side against a scripted guest: what it accepts and what it refuses.

use std::os::unix::net::UnixStream;

use kbf_guest::client::{Client, ClientError};
use kbf_guest::wire::{
    End, Exit, GuestMsg, HostMsg, Refusal, RunRequest, TOKEN_LEN, Usage, VERSION, read_frame,
    write_frame,
};

/// A guest that reads one message per reply and sends the reply.
fn guest(replies: Vec<GuestMsg>) -> (UnixStream, std::thread::JoinHandle<Vec<HostMsg>>) {
    let (host, mut guest) = UnixStream::pair().expect("pair");
    let thread = std::thread::spawn(move || {
        let mut got = Vec::new();
        for reply in replies {
            let frame = read_frame(&mut guest).expect("host message");
            got.push(HostMsg::decode(&frame).expect("decodes"));
            write_frame(&mut guest, &reply.encode()).expect("reply");
        }
        got
    });
    (host, thread)
}

fn ready() -> GuestMsg {
    GuestMsg::Ready {
        version: VERSION,
        session: "Aqua".into(),
    }
}

fn exit() -> GuestMsg {
    GuestMsg::Exited(Exit {
        end: End::Exited(0),
        usage: Usage::default(),
        outputs: Vec::new(),
        stragglers: false,
    })
}

/// Catches a client that talks on to a guest of another version.
#[test]
fn a_guest_of_another_version_is_refused() {
    let (host, _guest) = guest(vec![GuestMsg::Ready {
        version: VERSION + 1,
        session: String::new(),
    }]);
    let err = Client::connect(host, [0; TOKEN_LEN]).expect_err("refused");
    assert!(
        matches!(err, ClientError::Version(v) if v == VERSION + 1),
        "{err}"
    );
}

/// Catches a reply out of turn taken as the one asked for: a `Started` for the
/// handshake, an `Exited` for a run, a `Started` for a wait.
#[test]
fn a_reply_out_of_turn_is_an_error() {
    let (host, _guest) = guest(vec![GuestMsg::Started { pid: 1 }]);
    let err = Client::connect(host, [0; TOKEN_LEN]).expect_err("out of turn");
    assert!(
        matches!(err, ClientError::Unexpected(GuestMsg::Started { .. })),
        "{err}"
    );

    let (host, _guest) = guest(vec![ready(), exit()]);
    let mut client = Client::connect(host, [7; TOKEN_LEN]).expect("handshake");
    assert_eq!(client.session(), "Aqua");
    let err = client.run(&RunRequest::default()).expect_err("out of turn");
    assert!(
        matches!(err, ClientError::Unexpected(GuestMsg::Exited(_))),
        "{err}"
    );

    let (host, guest) = guest(vec![
        ready(),
        GuestMsg::Started { pid: 9 },
        GuestMsg::Started { pid: 9 },
    ]);
    let mut client = Client::connect(host, [7; TOKEN_LEN]).expect("handshake");
    assert_eq!(client.run(&RunRequest::default()).expect("started"), 9);
    client.killer().expect("killer").kill().expect("sent");
    let err = client.wait().expect_err("out of turn");
    assert!(
        matches!(err, ClientError::Unexpected(GuestMsg::Started { .. })),
        "{err}"
    );
    // What the guest received: the token in Hello, the Run, the Kill.
    let got = guest.join().expect("guest");
    assert_eq!(
        got,
        vec![
            HostMsg::Hello {
                version: VERSION,
                token: [7; TOKEN_LEN]
            },
            HostMsg::Run(RunRequest::default()),
            HostMsg::Kill,
        ]
    );
}

/// Catches a refusal reported as another error, or its reason or detail lost, and a
/// run that exits not waited for: the guest's `Refused` to the handshake and to a `Run`
/// is `ClientError::Refused` with both, and an `Exited` after a started run is the wait's
/// answer.
#[test]
fn a_refusal_is_reported_as_it_was_said_and_an_exit_is_waited_for() {
    let (host, _guest) = guest(vec![GuestMsg::Refused {
        reason: Refusal::Token,
        detail: "wrong token".into(),
    }]);
    let err = Client::connect(host, [0; TOKEN_LEN]).expect_err("refused");
    assert!(
        matches!(&err, ClientError::Refused { reason: Refusal::Token, detail } if detail == "wrong token"),
        "{err}"
    );

    let (host, _guest) = guest(vec![
        ready(),
        GuestMsg::Refused {
            reason: Refusal::AlreadyRan,
            detail: "this boot already ran a command".into(),
        },
    ]);
    let mut client = Client::connect(host, [7; TOKEN_LEN]).expect("handshake");
    let err = client.run(&RunRequest::default()).expect_err("refused");
    assert!(
        matches!(&err, ClientError::Refused { reason: Refusal::AlreadyRan, detail }
            if detail == "this boot already ran a command"),
        "{err}"
    );

    // The guest answers its third read, the `Kill`, with the exit.
    let (host, guest) = guest(vec![ready(), GuestMsg::Started { pid: 9 }, exit()]);
    let mut client = Client::connect(host, [7; TOKEN_LEN]).expect("handshake");
    assert_eq!(client.run(&RunRequest::default()).expect("started"), 9);
    client.killer().expect("killer").kill().expect("sent");
    let exited = client.wait().expect("exited");
    assert_eq!(exited.end, End::Exited(0));
    guest.join().expect("guest");
}
