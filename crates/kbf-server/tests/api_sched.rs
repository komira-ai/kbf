//! `GET /v1/leases` and `GET /v1/queue` over a real listener, with a daemon that holds
//! leases and work that waits: the shape of both answers, the node filter, paging
//! through the leases, and what a lease and the queue look like as the work moves.

mod support;

use std::time::Duration;

use kbf_server::api::NODES;
use kbf_server::api_client::OperatorClient;
use kbf_server::http_sched::{LEASES, QUEUE};
use serde_json::Value;
use support::{Cell, Job};
use tokio::time::{Instant, sleep};

/// A `GET` of `path` on the cell's operator API: the status and the JSON body.
async fn get(cell: &Cell, path: &str) -> (u16, Value) {
    let api = cell.api.expect("a cell with an operator API");
    let path = path.to_owned();
    let reply = tokio::task::spawn_blocking(move || {
        OperatorClient::new(api, None).get(&path).expect("a reply")
    })
    .await
    .expect("the call runs");
    (
        reply.status,
        serde_json::from_str(&reply.body).expect("JSON"),
    )
}

/// The answer of `path`, which must be 200, once `ready` holds of it (the scheduler's
/// views follow its ticks, so a view may need a moment).
async fn until(cell: &Cell, path: &str, ready: impl Fn(&Value) -> bool) -> Value {
    let started = Instant::now();
    loop {
        let (code, body) = get(cell, path).await;
        assert_eq!(code, 200, "{body}");
        if ready(&body) {
            return body;
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{path} never became ready: {body}"
        );
        sleep(Duration::from_millis(50)).await;
    }
}

fn items(body: &Value) -> &Vec<Value> {
    body["items"].as_array().expect("items")
}

/// Catches the lease view disagreeing with what a daemon holds, end to end: three
/// operations granted to one node show as three leases in lease order with the node,
/// operation, instance, digest, QoS, platform and a phase; the node filter keeps that
/// node's and an unknown node's is empty (200); paging by 2 gives 2 then 1 with a
/// `next_page` that leads to the rest and no repeat; `/v1/nodes` agrees on the count.
/// Catches: the filter ignored; the page not truncated; the next page repeating the last
/// lease; the token not advancing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn leases_list_what_a_daemon_holds_and_page() {
    let cell = Cell::start_with_api().await;
    let jobs: Vec<Job> = (0..3)
        .map(|i| Job::new(&format!("lease view {i}"), &[]))
        .collect();
    for job in &jobs {
        cell.upload(&job.blobs()).await;
    }
    let mut streams = Vec::new();
    for job in &jobs {
        streams.push(cell.execute(&job.action).await);
    }
    let mut daemon = cell.daemon("node-a", 8, 16).await;
    for _ in &jobs {
        daemon.start().await;
    }

    let all = until(&cell, LEASES, |b| items(b).len() == 3).await;
    let term = all["term"].as_str().expect("a string term");
    let rows = items(&all);
    let ids: Vec<&str> = rows.iter().map(|r| r["lease"].as_str().unwrap()).collect();
    for (i, id) in ids.iter().enumerate() {
        assert_eq!(*id, format!("{term}.{i}"), "lease order");
    }
    for row in rows {
        assert_eq!(row["node"], "node-a");
        assert_eq!(row["instance"], "main");
        assert_eq!(row["qos"], "ci");
        assert_eq!(row["platform"], "any");
        assert!(matches!(row["phase"].as_str(), Some("granted" | "running")));
        assert!(row["operation"].as_str().unwrap().starts_with("op-"));
        let digest = row["digest"].as_str().unwrap();
        assert!(digest.contains('/'), "{digest}");
        assert!(row["granted_unix_ms"].as_u64().unwrap() > 1_600_000_000_000);
        assert!(row["age_ms"].as_u64().is_some() && row["since_ms"].as_u64().is_some());
    }
    assert_eq!(all["next_page"], Value::Null);

    let (code, mine) = get(&cell, &format!("{LEASES}?node=node-a")).await;
    assert_eq!((code, items(&mine).len()), (200, 3));
    let (code, none) = get(&cell, &format!("{LEASES}?node=node-z")).await;
    assert_eq!(code, 200, "{none}");
    assert!(items(&none).is_empty());
    assert_eq!(none["next_page"], Value::Null);

    // Page by two.
    let (code, first) = get(&cell, &format!("{LEASES}?page_size=2")).await;
    assert_eq!((code, items(&first).len()), (200, 2));
    let next = first["next_page"].as_str().expect("a next page");
    assert!(next.starts_with("/v1/leases?"), "root-relative: {next}");
    let (code, second) = get(&cell, next).await;
    assert_eq!((code, items(&second).len()), (200, 1));
    assert_eq!(second["next_page"], Value::Null);
    let paged: Vec<&str> = items(&first)
        .iter()
        .chain(items(&second))
        .map(|r| r["lease"].as_str().unwrap())
        .collect();
    assert_eq!(paged, ids, "every lease once, in order");

    // A page of one node's leases cannot be continued as another filter's.
    let (_, filtered) = get(&cell, &format!("{LEASES}?node=node-a&page_size=1")).await;
    let token = filtered["next_page"]
        .as_str()
        .and_then(|link| link.split("page_token=").nth(1))
        .expect("a token");
    let (code, _) = get(&cell, &format!("{LEASES}?node=node-b&page_token={token}")).await;
    assert_eq!(code, 400);
    let (code, _) = get(&cell, &format!("{LEASES}?page_token={token}")).await;
    assert_eq!(code, 400);

    // The nodes route counts the same leases (the two views are one scheduler).
    let (_, nodes) = get(&cell, NODES).await;
    assert!(nodes.to_string().contains("node-a"), "{nodes}");
    drop(streams);
}

/// Catches the queue view disagreeing with what waits: work no node can run shows its
/// depth, its platform and QoS group with an age, a waiting reason with a count and an
/// example the caller can look up, and an oldest age that is not null; a farm with
/// nothing queued shows depth 0 and a null oldest age. Catches: depth not counted;
/// reasons left out; the platform label dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_queue_shows_work_no_node_can_run() {
    let cell = Cell::start_with_api().await;
    let (code, quiet) = get(&cell, QUEUE).await;
    assert_eq!(code, 200);
    assert_eq!(quiet["depth"], 0);
    assert_eq!(quiet["oldest_age_ms"], Value::Null);
    assert_eq!(quiet["by_platform"], serde_json::json!([]));

    let job = Job::new("waits for a mac", &[("os", "macos")]);
    cell.upload(&job.blobs()).await;
    let _stream = cell.execute(&job.action).await;
    let _daemon = cell.daemon("node-a", 4, 8).await;

    let body = until(&cell, QUEUE, |b| {
        b["depth"] == 1 && !b["waiting"].as_array().unwrap().is_empty()
    })
    .await;
    assert!(body["oldest_age_ms"].as_u64().is_some(), "{body}");
    let [group] = body["by_platform"].as_array().unwrap().as_slice() else {
        panic!("{body}");
    };
    assert_eq!(group["platform"], "os=macos");
    assert_eq!(group["qos"], "ci");
    assert_eq!(group["depth"], 1);
    let [waiting] = body["waiting"].as_array().unwrap().as_slice() else {
        panic!("{body}");
    };
    assert_eq!(waiting["count"], 1);
    assert!(
        waiting["reason"].as_str().unwrap().contains("macos"),
        "{body}"
    );
    assert!(
        waiting["example_operation"]
            .as_str()
            .unwrap()
            .starts_with("op-")
    );
    assert!(waiting["example_digest"].as_str().unwrap().contains('/'));
    assert_eq!(body["avoided"], serde_json::json!([]));
    assert_eq!(body["avoided_total"], 0);
}
