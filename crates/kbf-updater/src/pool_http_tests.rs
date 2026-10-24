//! The pool adapter against a fake admin API of the proxy on loopback.

use std::time::Duration;

use crate::operator::OperatorApi;
use crate::pool_http::{CHECK, HttpPool, NOT_CONFIGURED};
use crate::ports::{Pool, PoolMember, PortError, PortErrorKind};
use crate::testutil::{FakeApi, closed_port, fake_api, http};

fn answering(status: u16, body: &str) -> FakeApi {
    fake_api(vec![Some(http(status, body))])
}

fn pool_at(server: &FakeApi) -> HttpPool {
    HttpPool::new(server.addr)
}

const LIST: &str = r#"[
  {"name":"a","state":"up","ready":true,
   "open_streams":{"reapi":2,"worker":7,"operator":3,"enroll":1}},
  {"name":"b","state":"draining","ready":false,
   "open_streams":{"reapi":0,"worker":0,"operator":0,"enroll":0}},
  {"name":"c","state":"paused","ready":true,
   "open_streams":{"reapi":0,"worker":0,"operator":1,"enroll":0}}
]"#;

/// Asserts that the view is a `GET` of the proxy's list, with no credential of any kind, and
/// that each member's state, readiness and REAPI and operator streams are read from it, a
/// state other than `up` being read as draining.
#[test]
fn the_view_reads_each_member_and_sends_no_credential() {
    let server = answering(200, LIST);
    let view = pool_at(&server).view().expect("view");
    let member = |name: &str, draining, ready, reapi, operator| PoolMember {
        name: name.to_owned(),
        draining,
        ready,
        reapi_streams: reapi,
        operator_streams: operator,
    };
    let want = [
        member("a", false, true, 2, 3),
        member("b", true, false, 0, 0),
        member("c", true, true, 0, 1),
    ];
    assert_eq!(view, want);
    let request = server.requests().remove(0);
    assert!(request.starts_with("GET /v1/proxy/upstreams HTTP/1.1\r\n"));
    let lower = request.to_lowercase();
    assert!(!lower.contains("authorization"), "{request}");
    assert!(!lower.contains("x-kbf-proxy-auth"), "{request}");
}

/// Asserts that a drain and an undrain are `POST`s of the member's own route, empty, without
/// a credential, and that a 200 is all they need.
#[test]
fn a_drain_and_an_undrain_post_the_members_route() {
    let server = answering(200, LIST);
    let pool = pool_at(&server);
    assert_eq!(pool.drain("a"), Ok(()));
    assert_eq!(pool.undrain("a-1"), Ok(()));
    let seen = server.requests();
    assert!(seen[0].starts_with("POST /v1/proxy/upstreams/a:drain HTTP/1.1\r\n"));
    assert!(seen[1].starts_with("POST /v1/proxy/upstreams/a-1:undrain HTTP/1.1\r\n"));
    let lower = seen[0].to_lowercase();
    assert!(!lower.contains("authorization"), "{lower}");
}

/// Asserts that each answer that is not what was asked for is an error of the right kind: a
/// 404 says the proxy has no such member, a refusal names `--proxy-admin`, a 5xx and a port
/// nothing listens at are no answer, and a list that is not a list is garbled. No text
/// carries the address.
#[test]
fn refusals_failures_and_garbled_lists_are_errors_of_the_right_kind() {
    let cases = [
        (404, "{}", PortErrorKind::Other, "has no member a"),
        (403, "{}", PortErrorKind::Other, "refused the call (403)"),
        (500, "{}", PortErrorKind::Unreachable, "answered 500"),
    ];
    for (status, body, kind, text) in cases {
        let server = answering(status, body);
        let pool = pool_at(&server);
        for result in [pool.drain("a"), pool.undrain("a")] {
            let e = result.expect_err(text);
            assert_eq!(e.kind(), kind, "{e}");
            assert!(e.to_string().contains(text), "{e}");
            assert!(!e.to_string().contains(&server.addr.to_string()), "{e}");
        }
    }
    let server = answering(403, "{}");
    let e = pool_at(&server).view().expect_err("refused");
    assert!(e.to_string().contains(CHECK), "{e}");
    assert!(e.to_string().contains("the proxy's admin API"), "{e}");
    let server = answering(200, r#"{"members":[]}"#);
    let e = pool_at(&server).view().expect_err("garbled");
    assert!(e.to_string().contains("list is garbled"), "{e}");
    let timeout = Duration::from_millis(500);
    let api = OperatorApi::anonymous(closed_port()).with_timeout(timeout);
    let pool = HttpPool::with_api(api);
    assert!(pool.view().expect_err("dead").is_unreachable());
    assert!(pool.drain("a").expect_err("dead").is_unreachable());
}

/// Asserts that a name the host would not take as a unit instance is refused before any
/// request, so that no path is made of free text.
#[test]
fn a_name_that_is_not_a_member_name_makes_no_request() {
    let server = answering(200, LIST);
    let pool = pool_at(&server);
    for name in ["", "A", "a/b", "a:drain", "../x", "a b", "-a"] {
        let e = pool.drain(name).expect_err(name);
        assert_eq!(e.kind(), PortErrorKind::Other);
        assert!(e.to_string().contains("is not a member name"), "{e}");
        assert!(pool.undrain(name).is_err());
    }
    assert!(server.requests().is_empty());
}

/// Asserts that a pool made without `--proxy-admin` answers every call with the same error
/// that says so, and asks nothing.
#[test]
fn a_pool_that_is_not_configured_says_so() {
    let pool = HttpPool::unconfigured();
    let said = PortError::other(NOT_CONFIGURED);
    assert_eq!(pool.view(), Err(said.clone()));
    assert_eq!(pool.drain("a"), Err(said.clone()));
    assert_eq!(pool.undrain("a"), Err(said));
}
