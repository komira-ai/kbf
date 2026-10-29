use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use kbf_front::Cache;
use kbf_sched::{Avoided, LeasePhase, QueueGroup, Unservable, WaitingGroup};
use kbf_types::{ActionKey, Digest, DigestFunction, FarmTime, OperationId, Qos, WorkKey};
use tower_service::Service;

use super::*;

fn digest(n: u8) -> Digest {
    Digest::new(DigestFunction::Sha256, [n; 32], u64::from(n))
}

/// The key of action `digest(n)` on instance `main`.
fn key(n: u8) -> WorkKey {
    ActionKey {
        instance: "main".to_owned(),
        action: digest(n),
    }
    .into()
}

fn query(q: &str) -> Result<Query, String> {
    Query::parse(q)
}

/// Catches a query read loosely: the defaults, the bounds of `page_size` (0, past the
/// maximum, not a number), an empty or oversized node, a repeated key, an unknown key (a
/// typo that silently filters nothing), and a token made for another filter or not made
/// by this route. Catches: the page-size bound off by one; a repeated key taken as the
/// last; the filter left out of the token.
#[test]
fn a_query_is_checked() {
    assert_eq!(
        query(""),
        Ok(Query {
            node: None,
            size: DEFAULT_PAGE_SIZE,
            after: None
        })
    );
    assert_eq!(query("page_size=1").unwrap().size, 1);
    assert_eq!(
        query(&format!("page_size={MAX_PAGE_SIZE}")).unwrap().size,
        200
    );
    assert_eq!(
        query("node=mac%2007").unwrap().node.as_deref(),
        Some("mac 07")
    );
    let long = format!("node={}", "n".repeat(MAX_NODE_LEN));
    assert!(query(&long).is_ok());
    let too_long = format!("node={}", "n".repeat(MAX_NODE_LEN + 1));
    for bad in [
        "page_size=0",
        &format!("page_size={}", MAX_PAGE_SIZE + 1),
        "page_size=many",
        "page_size=-1",
        "page_size=",
        "node=",
        too_long.as_str(),
        "node=a&node=b",
        "node=a&node=a",
        "page_size=1&page_size=2",
        "page_token=1.2.0000000000000000&page_token=1.2.0000000000000000",
        "size=3",
        "NODE=a",
        "page_token=garbage",
        "page_token=1.2",
        "page_token=1.2.zz",
        "page_token=a.2.0000000000000000",
    ] {
        assert!(query(bad).is_err(), "{bad}");
    }
}

/// Catches a page token that does not round-trip, or that is accepted for another node
/// filter (a page of one node's leases continued as another's, or as the whole list).
#[test]
fn a_token_names_its_lease_and_its_filter() {
    let lease = LeaseId::new(u64::MAX, 41);
    for node in [None, Some("node-a")] {
        let t = token(lease, node);
        assert_eq!(untoken(&t, node), Some(lease), "{t}");
        let parsed = query(&match node {
            Some(n) => format!("node={n}&page_token={t}"),
            None => format!("page_token={t}"),
        })
        .unwrap();
        assert_eq!(parsed.after, Some(lease));
    }
    let made_for_a = token(lease, Some("node-a"));
    assert_eq!(untoken(&made_for_a, Some("node-b")), None);
    assert_eq!(untoken(&made_for_a, None), None);
    assert_eq!(untoken(&token(lease, None), Some("node-a")), None);
    assert_ne!(token(lease, Some("node-a")), token(lease, Some("node-b")));
}

fn view() -> LeaseView {
    LeaseView {
        lease: LeaseId::new(7, 3),
        operation: OperationId(12),
        key: key(9),
        node: kbf_types::WorkerId::new("node-a"),
        platform: "os=linux".to_owned(),
        qos: Qos::Ci,
        phase: LeasePhase::Running,
        granted_at: FarmTime::from_millis(1_000),
        started_at: Some(FarmTime::from_millis(2_500)),
        age: Duration::from_millis(4_000),
        phase_age: Duration::from_millis(2_500),
    }
}

/// Catches a lease item that renames, drops or mixes up a field, or shows a farm time
/// as one: the exact shape, the times as the epoch plus the farm time, and an unstarted
/// lease's start as `null`, not zero. Catches: the epoch not added; `age_ms` and
/// `since_ms` swapped; the start of an unstarted lease shown as the grant.
#[test]
fn a_lease_item_has_the_documented_shape() {
    let item = lease_item(&view(), 1_700_000_000_000);
    assert_eq!(
        item,
        json!({
            "lease": "7.3",
            "node": "node-a",
            "phase": "running",
            "age_ms": 4000,
            "since_ms": 2500,
            "granted_unix_ms": 1_700_000_001_000_u64,
            "started_unix_ms": 1_700_000_002_500_u64,
            "operation": "op-12",
            "instance": "main",
            "digest": digest(9).to_string(),
            "platform": "os=linux",
            "qos": "ci",
        })
    );
    let mut unstarted = view();
    unstarted.phase = LeasePhase::Granted;
    unstarted.started_at = None;
    let item = lease_item(&unstarted, 0);
    assert_eq!(item["started_unix_ms"], Value::Null);
    assert_eq!(item["phase"], "granted");
}

/// Catches a queue body that renames or drops a field, shows a missing oldest age as
/// zero, or loses the total of the avoided list.
#[test]
fn the_queue_body_has_the_documented_shape() {
    let empty = QueueView {
        depth: 0,
        oldest_age: None,
        groups: Vec::new(),
        waiting: Vec::new(),
        avoided: Vec::new(),
        avoided_total: 0,
    };
    assert_eq!(
        queue_body(&empty, None),
        json!({
            "depth": 0, "oldest_age_ms": null, "by_platform": [], "waiting": [],
            "avoided": [], "avoided_total": 0,
        })
    );
    let full = QueueView {
        depth: 5,
        oldest_age: Some(Duration::from_millis(61_500)),
        groups: vec![QueueGroup {
            platform: "os=macos".to_owned(),
            qos: Qos::Batch,
            depth: 5,
            oldest_age: Duration::from_millis(61_500),
        }],
        waiting: vec![WaitingGroup {
            reason: "none of the 2 live workers satisfies the action's platform".to_owned(),
            cause: Unservable::PlatformMismatch {
                serving: 2,
                some_cannot_serve: false,
                kind: kbf_types::LeaseKind::Action,
                closest: kbf_types::WorkerId::new("a"),
                lacks: vec!["os=macos".to_owned()],
            },
            count: 4,
            example: OperationId(3),
            example_key: key(1),
        }],
        avoided: vec![Avoided {
            operation: OperationId(4),
            key: key(2),
            nodes: vec![kbf_types::WorkerId::new("a"), kbf_types::WorkerId::new("b")],
        }],
        avoided_total: 250,
    };
    assert_eq!(
        queue_body(&full, None),
        json!({
            "depth": 5,
            "oldest_age_ms": 61_500,
            "by_platform": [{
                "platform": "os=macos", "qos": "batch", "depth": 5, "oldest_age_ms": 61_500
            }],
            "waiting": [{
                "reason": "none of the 2 live workers satisfies the action's platform",
                "count": 4,
                "example_operation": "op-3",
                "example_instance": "main",
                "example_digest": digest(1).to_string(),
            }],
            "avoided": [{
                "operation": "op-4", "instance": "main",
                "digest": digest(2).to_string(), "nodes": ["a", "b"]
            }],
            "avoided_total": 250,
        })
    );
}

async fn get(path: &str) -> (StatusCode, axum::http::HeaderMap, Value) {
    let farm = Arc::new(Farm::new(
        Arc::new(Cache::memory()),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    ));
    let mut app = router(farm);
    let request = Request::builder().uri(path).body(Body::empty()).unwrap();
    let answer = app.call(request).await.unwrap();
    let (parts, body) = answer.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
    (
        parts.status,
        parts.headers,
        serde_json::from_slice(&bytes).unwrap(),
    )
}

/// Catches routes that are not mounted, answer an empty farm with an error or a body of
/// another shape, or can be cached: both answer 200 with `Cache-Control: no-store`, an
/// empty lease list with no next page, an unknown node's leases as empty (not 404), and a
/// quiet queue with a null oldest age. Catches: `no-store` left off one route; an unknown
/// node answered 404.
#[tokio::test]
async fn an_empty_farm_answers_both_routes_uncached() {
    let (code, headers, body) = get(LEASES).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(headers[CACHE_CONTROL], "no-store");
    assert_eq!(
        headers[axum::http::header::CONTENT_TYPE],
        "application/json"
    );
    let term = body["term"].as_str().expect("term is a string");
    assert!(term.parse::<u64>().is_ok(), "{term}");
    assert_eq!(body["items"], json!([]));
    assert_eq!(body["next_page"], Value::Null);

    let (code, _, body) = get("/v1/leases?node=never-registered&page_size=1").await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["items"], json!([]));

    let (code, headers, body) = get(QUEUE).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(headers[CACHE_CONTROL], "no-store");
    assert_eq!(body["depth"], 0);
    assert_eq!(body["oldest_age_ms"], Value::Null);
}

/// Catches a bad request answered with anything but 400 and the stable error body, or
/// cached, and a queue route that ignores a query it does not take.
#[tokio::test]
async fn bad_queries_are_400_and_uncached() {
    for path in [
        "/v1/leases?page_size=0",
        "/v1/leases?page_size=201",
        "/v1/leases?bogus=1",
        "/v1/leases?node=a&node=b",
        "/v1/leases?page_token=nonsense",
        "/v1/queue?page_size=5",
        "/v1/queue?node=a",
    ] {
        let (code, headers, body) = get(path).await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "{path}");
        assert_eq!(headers[CACHE_CONTROL], "no-store", "{path}");
        assert!(
            body["error"].as_str().is_some_and(|e| !e.is_empty()),
            "{path}"
        );
    }
    // A bare `?` is no query.
    let (code, _, _) = get("/v1/queue?").await;
    assert_eq!(code, StatusCode::OK);
}
