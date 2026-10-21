//! The frontend's operator API, read with a plain HTTP/1.1 request: the pages the
//! harness waits on (`/readyz`, the nodes and members it lists) and `/metrics`.

use std::io::{Read as _, Write as _};
use std::time::Duration;

/// `GET path` of the operator API at `127.0.0.1:api`: the status code and the body, or
/// `None` while nothing answers.
#[must_use]
pub fn get(api: u16, path: &str) -> Option<(u16, String)> {
    request(
        api,
        &format!("GET {path} HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\n\r\n"),
    )
}

/// `POST path` with the JSON `body` to the operator API at `127.0.0.1:api`, as an operator
/// does: the bearer `token`, and no `Origin` (a browser's request has one, and is refused).
/// The status code and the body, or `None` while nothing answers.
#[must_use]
pub fn post(api: u16, token: &str, path: &str, body: &str) -> Option<(u16, String)> {
    request(
        api,
        &format!(
            "POST {path} HTTP/1.1\r\nHost: kbf\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
    )
}

/// Sends `request` whole and reads the answer to its end.
fn request(api: u16, request: &str) -> Option<(u16, String)> {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], api));
    let mut stream = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(5)).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .ok()?;
    // One write: a request in pieces may be answered before it is whole.
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

/// One storage member as `GET /v1/members` lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    /// The number `POST /v1/members/{id}` takes.
    pub id: u64,
    /// The member's storage address as the map has it (`host:port`, with a scheme or not).
    pub addr: String,
    /// `active`, `catching_up`, `planned_down`, `down`, ...
    pub state: String,
}

/// The members `GET /v1/members` lists, in its order; `None` while the frontend does not
/// answer or answers in another shape.
#[must_use]
pub fn members(api: u16) -> Option<Vec<Member>> {
    let (code, body) = get(api, "/v1/members")?;
    (code == 200).then_some(())?;
    let view: serde_json::Value = serde_json::from_str(&body).ok()?;
    view["members"]
        .as_array()?
        .iter()
        .map(|m| {
            let text = |key: &str| m[key].as_str().map(str::to_owned);
            Some(Member {
                id: m["id"].as_u64()?,
                addr: text("addr")?,
                state: text("state")?,
            })
        })
        .collect()
}

#[cfg(test)]
#[path = "api_tests.rs"]
pub(crate) mod tests;
