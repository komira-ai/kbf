//! The frontend member adapter against a fake operator API on loopback.

use std::net::SocketAddr;
use std::time::Duration;

use crate::fakes::draining;
use crate::frontend_http::HttpFrontend;
use crate::inventory::{Host, Member, Role};
use crate::operator::OperatorApi;
use crate::ports::{FrontendControl, PortError, PortErrorKind, RetireStatus};
use crate::testutil::{
    FakeApi, KEY, TOKEN, closed_port, fake_api, fake_api_by, file, http, scratch,
};

/// A frontend member whose operator port is the port of `addr`.
fn member_host(addr: SocketAddr) -> Host {
    Host {
        alias: "fe-a".to_owned(),
        address: "secret-host.farm.example".to_owned(),
        role: Role::Frontend,
        host_key: KEY.to_owned(),
        node_id: None,
        member: Some(Member {
            instance: "a".to_owned(),
            base_port: addr.port() - 2,
        }),
    }
}

fn adapter(name: &str) -> HttpFrontend {
    let token = file(&scratch(name), "token", TOKEN, 0o600);
    let api = OperatorApi::new(closed_port(), token, None).expect("api");
    HttpFrontend::with_api(api.with_timeout(Duration::from_millis(500)))
}

fn answering(status: u16, body: &str) -> FakeApi {
    fake_api(vec![Some(http(status, body))])
}

const RETIRED: &str = r#"{"retired":true,"open_streams":0,"open_sessions":2,"more":1}"#;

/// Asserts that a retire is a `POST` of the retire route to the member's own operator port,
/// with the operator token, that its status is read from the answer, and that the status is a
/// `GET` of the same route.
#[test]
fn a_retire_is_posted_to_the_members_own_operator_port() {
    let server = answering(200, RETIRED);
    let front = adapter("fh-retire");
    let host = member_host(server.addr);
    let want = RetireStatus {
        retired: true,
        open_streams: 0,
        open_sessions: 2,
    };
    assert_eq!(front.retire(&host), Ok(want));
    assert_eq!(front.retire_status(&host), Ok(want));
    let seen = server.requests();
    assert!(seen[0].starts_with("POST /v1/frontend/retire HTTP/1.1\r\n"));
    assert!(seen[1].starts_with("GET /v1/frontend/retire HTTP/1.1\r\n"));
    for request in &seen {
        assert!(request.contains(&format!("Bearer {TOKEN}")), "{request}");
    }
}

/// Asserts that the `POST` of the retire is given longer to be answered than the client's bound
/// for a read: a member that answers after 700 ms, with the bound at 500 ms, is retired and
/// reports its status, while a status read of it times out as an unreachable member.
#[test]
fn the_retire_is_given_longer_to_answer_than_a_read() {
    let server = fake_api_by(|_, _| {
        std::thread::sleep(Duration::from_millis(700));
        Some(http(200, RETIRED))
    });
    let front = adapter("fh-slow");
    let host = member_host(server.addr);
    assert_eq!(front.retire(&host).map(|s| s.retired), Ok(true));
    let e = front.retire_status(&host).expect_err("timed out");
    assert!(e.is_unreachable(), "{e}");
}

/// Asserts that the commit is the one `GET /healthz` reports.
#[test]
fn the_commit_is_the_one_healthz_reports() {
    let body = r#"{"status":"alive","commit":"0123456789ab"}"#;
    let server = answering(200, body);
    let host = member_host(server.addr);
    let front = adapter("fh-commit");
    assert_eq!(front.commit(&host), Ok("0123456789ab".to_owned()));
    assert!(server.requests()[0].starts_with("GET /healthz HTTP/1.1\r\n"));
}

/// Asserts that each answer that is not a status is an error of the right kind and says what
/// went wrong: a 409 says there is no retire, a refusal of the gates names what to check, a 5xx
/// and a port nothing listens at are no answer, and a body that is not a status is garbled. No
/// text carries the token or the host's address.
#[test]
fn refusals_failures_and_garbled_answers_are_errors_of_the_right_kind() {
    let cases = [
        (409, "{}", PortErrorKind::Other, "has no retire"),
        (403, "{}", PortErrorKind::Other, "refused the call (403)"),
        (500, "{}", PortErrorKind::Unreachable, "answered 500"),
        (200, "[]", PortErrorKind::Other, "garbled retire status"),
    ];
    let front = adapter("fh-errors");
    for (status, body, kind, text) in cases {
        let server = answering(status, body);
        let host = member_host(server.addr);
        for result in [front.retire(&host), front.retire_status(&host)] {
            let e = result.expect_err(text);
            assert_eq!(e.kind(), kind, "{e}");
            assert!(e.to_string().contains(text), "{e}");
            for secret in [TOKEN, "secret-host", &server.addr.to_string()] {
                assert!(!e.to_string().contains(secret), "{secret} in {e}");
            }
        }
    }
    let dead = member_host(closed_port());
    assert!(front.retire(&dead).expect_err("refused").is_unreachable());
}

/// Asserts that the commit fails as the retire does: a status other than 200 is read as the
/// others are, and a body without a commit is garbled.
#[test]
fn a_commit_that_cannot_be_read_is_an_error() {
    let front = adapter("fh-commit-errors");
    let down = answering(503, "{}");
    let host = member_host(down.addr);
    let e = front.commit(&host).expect_err("5xx");
    assert_eq!(e.kind(), PortErrorKind::Unreachable);
    let garbled = answering(200, r#"{"status":"alive"}"#);
    let host = member_host(garbled.addr);
    let e = front.commit(&host).expect_err("garbled");
    assert!(e.to_string().contains("garbled health report"), "{e}");
}

const DRAINING: &str = r#"{"draining":true,"open_executions":2,"queued_operations":1}"#;

/// Asserts that a drain is a `POST` of the drain route to the member's own operator port, with
/// the operator token, that its status is a `GET` of the same route read as the draining flag
/// and the open executions, with `closing` false when the member does not say and true when it
/// does, and that a status the member does not know the fields of is still read.
#[test]
fn a_drain_is_posted_and_its_status_is_read_from_the_members_own_port() {
    let server = answering(200, DRAINING);
    let front = adapter("fh-drain");
    let host = member_host(server.addr);
    assert_eq!(front.drain(&host), Ok(()));
    assert_eq!(front.drain_status(&host), Ok(draining(2)));
    let seen = server.requests();
    assert!(seen[0].starts_with("POST /v1/frontend/drain HTTP/1.1\r\n"));
    assert!(seen[1].starts_with("GET /v1/frontend/drain HTTP/1.1\r\n"));
    for request in &seen {
        assert!(request.contains(&format!("Bearer {TOKEN}")), "{request}");
    }
    let body = r#"{"draining":true,"open_executions":0,"phase":"closing","new":1}"#;
    let closing = answering(200, body);
    let status = front
        .drain_status(&member_host(closing.addr))
        .expect("closing");
    assert!(status.closing);
    assert_eq!(status.open_executions, 0);
}

/// Asserts that the readiness is a `GET /readyz` to the member's own operator port, that a 200
/// is ready and a 503 is not ready and neither is an error.
#[test]
fn the_readiness_is_200_for_ready_and_503_for_not_ready() {
    let front = adapter("fh-ready");
    let up = answering(200, "{}");
    assert_eq!(front.ready(&member_host(up.addr)), Ok(true));
    assert!(up.requests()[0].starts_with("GET /readyz HTTP/1.1\r\n"));
    let down = answering(503, "{}");
    assert_eq!(front.ready(&member_host(down.addr)), Ok(false));
}

/// Asserts that each answer to the drain, its status and the readiness that is not what was
/// asked for is an error of the right kind that is not a refused connection: a refusal of the
/// gates names what to check, a 5xx is no answer, anything else is a surprise, a status that is
/// not a drain status is garbled, and a 5xx to the readiness is no answer and not a 503. No text
/// carries the token or the host's address.
#[test]
fn failures_of_the_drain_and_the_readiness_are_errors_of_the_right_kind() {
    let cases = [
        (403, PortErrorKind::Other, "refused the call (403)"),
        (500, PortErrorKind::Unreachable, "answered 500"),
        (404, PortErrorKind::Other, "answered 404"),
    ];
    let front = adapter("fh-drain-errors");
    for (status, kind, text) in cases {
        let server = answering(status, "{}");
        let host = member_host(server.addr);
        let results: [Result<(), PortError>; 3] = [
            front.drain(&host),
            front.drain_status(&host).map(drop),
            front.ready(&host).map(drop),
        ];
        for result in results {
            let e = result.expect_err(text);
            assert_eq!(e.kind(), kind, "{e}");
            assert!(e.to_string().contains(text), "{e}");
            assert!(!e.is_refused(), "{e}");
            for secret in [TOKEN, "secret-host", &server.addr.to_string()] {
                assert!(!e.to_string().contains(secret), "{secret} in {e}");
            }
        }
    }
    let garbled = answering(200, "[]");
    let e = front
        .drain_status(&member_host(garbled.addr))
        .expect_err("garbled");
    assert!(e.to_string().contains("garbled drain status"), "{e}");
}

/// Asserts that a member that is not running is a refused connection for the drain, its status
/// and the readiness, and that a member that reads the request and does not answer in time is
/// unreachable and not refused: only the first is a member that has exited.
#[test]
fn a_member_that_is_gone_refuses_the_connection_and_a_slow_one_does_not() {
    let front = adapter("fh-gone");
    let dead = member_host(closed_port());
    let errors = [
        front.drain(&dead).expect_err("dead"),
        front.drain_status(&dead).expect_err("dead"),
        front.ready(&dead).expect_err("dead"),
    ];
    for e in errors {
        assert!(e.is_unreachable(), "{e}");
        assert!(e.is_refused(), "{e}");
    }
    let slow = fake_api(vec![None]);
    let e = front
        .drain_status(&member_host(slow.addr))
        .expect_err("timed out");
    assert!(e.is_unreachable(), "{e}");
    assert!(!e.is_refused(), "{e}");
}

/// Asserts that a host that is not a pool member is refused by every call, with its
/// alias, and that no request is made.
#[test]
fn a_host_that_is_no_member_is_refused_without_a_request() {
    let server = answering(200, RETIRED);
    let bare = Host {
        member: None,
        ..member_host(server.addr)
    };
    let front = adapter("fh-bare");
    let results: [Result<(), PortError>; 6] = [
        front.retire(&bare).map(drop),
        front.retire_status(&bare).map(drop),
        front.commit(&bare).map(drop),
        front.drain(&bare),
        front.drain_status(&bare).map(drop),
        front.ready(&bare).map(drop),
    ];
    for result in results {
        let e = result.expect_err("refused");
        assert_eq!(e, PortError::other("fe-a is not a pool member"));
    }
    assert!(server.requests().is_empty());
}
