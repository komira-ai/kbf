//! The frontend's operator API, read with a plain HTTP/1.1 request: the pages the
//! harness waits on (`/readyz`, the nodes and members it lists) and `/metrics`.

use std::io::{Read as _, Write as _};
use std::time::Duration;

/// `GET path` of the operator API at `127.0.0.1:api`: the status code and the body, or
/// `None` while nothing answers.
#[must_use]
pub fn get(api: u16, path: &str) -> Option<(u16, String)> {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], api));
    let mut stream = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(5)).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .ok()?;
    // One write: a request in pieces may be answered before it is whole.
    let request = format!("GET {path} HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).ok()?;
    let mut text = String::new();
    stream.read_to_string(&mut text).ok()?;
    let (head, body) = text.split_once("\r\n\r\n")?;
    let code = head.split_whitespace().nth(1)?.parse().ok()?;
    Some((code, body.to_owned()))
}

/// The ids of the nodes the frontend lists as connected, sorted; `None` while it does
/// not answer.
#[must_use]
pub fn connected(api: u16) -> Option<Vec<String>> {
    let (code, body) = get(api, "/v1/nodes")?;
    (code == 200).then_some(())?;
    let view: serde_json::Value = serde_json::from_str(&body).ok()?;
    let mut ids: Vec<String> = view["nodes"]
        .as_array()?
        .iter()
        .filter(|n| n["connected"] == true)
        .filter_map(|n| n["node_id"].as_str().map(str::to_owned))
        .collect();
    ids.sort();
    Some(ids)
}

/// How many storage members `GET /v1/members` lists, and whether every one is `active`: a
/// node that registered rebuilds its index first (`catching_up`), and each promotion is a
/// new map generation, which a client that wrote meanwhile would be told is stale.
#[must_use]
pub fn members_active(api: u16) -> Option<(usize, bool)> {
    let (code, body) = get(api, "/v1/members")?;
    (code == 200).then_some(())?;
    let view: serde_json::Value = serde_json::from_str(&body).ok()?;
    let members = view["members"].as_array()?;
    Some((
        members.len(),
        members.iter().all(|m| m["state"] == "active"),
    ))
}
