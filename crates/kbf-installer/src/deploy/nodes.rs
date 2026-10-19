//! Reading the operator API's answers. Every reader is tolerant of a field that is
//! missing (it reads as unknown) and strict about what counts as good: a missing field
//! never reads as a pass.

use serde_json::Value;

use super::summary::Previous;

/// One node, as `GET /v1/nodes` lists it.
pub struct Node {
    pub id: String,
    pub connected: bool,
    pub platform: Option<String>,
    pub serving: bool,
    pub state: String,
    pub running_commit: String,
    pub up_to_date: Option<bool>,
    pub availability: String,
    pub refusal: String,
    pub offered_generation: Option<u64>,
}

fn text(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_owned()
}

pub fn list(nodes: &Value) -> Vec<Node> {
    let Some(array) = nodes.get("nodes").and_then(Value::as_array) else {
        return Vec::new();
    };
    array
        .iter()
        .map(|n| {
            let update = n.get("update").unwrap_or(&Value::Null);
            Node {
                id: text(n, "node_id"),
                connected: n.get("connected").and_then(Value::as_bool) == Some(true),
                platform: platform_of(n),
                serving: n.pointer("/placement/state").and_then(Value::as_str) == Some("serving"),
                state: text(update, "state"),
                running_commit: text(update, "running_commit"),
                up_to_date: update.get("up_to_date").and_then(Value::as_bool),
                availability: text(update, "availability"),
                refusal: text(update, "refusal"),
                offered_generation: update.get("offered_generation").and_then(Value::as_u64),
            }
        })
        .collect()
}

/// The node's platform: `update.platform` if the API gives it, else its `os` and `arch`.
fn platform_of(node: &Value) -> Option<String> {
    let known = ["linux-x86_64", "linux-arm64", "darwin-arm64"];
    let given = node
        .pointer("/update/platform")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let derived = || {
        let soft = node.get("software")?;
        let os = soft.get("os").and_then(Value::as_str)?;
        let arch = soft.get("arch").and_then(Value::as_str)?;
        let os = match os {
            "macos" => "darwin",
            other => other,
        };
        Some(format!("{os}-{arch}"))
    };
    given
        .or_else(derived)
        .filter(|p| known.contains(&p.as_str()))
}

/// `complete`, `incomplete` (the commit is held, the platform is not complete) or `missing`.
pub fn platform_state(releases: &Value, commit: &str, platform: &str) -> &'static str {
    let found = releases
        .get("releases")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|r| r.get("commit").and_then(Value::as_str) == Some(commit))
        .flat_map(|r| {
            r.get("platforms")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
        })
        .find(|p| p.get("platform").and_then(Value::as_str) == Some(platform));
    let Some(p) = found else { return "missing" };
    let complete = p.get("complete").and_then(Value::as_bool) == Some(true);
    let attested = p
        .get("components")
        .and_then(Value::as_array)
        .is_some_and(|cs| {
            !cs.is_empty()
                && cs
                    .iter()
                    .all(|c| c.get("attested").and_then(Value::as_bool) == Some(true))
        });
    if complete && attested {
        "complete"
    } else {
        "incomplete"
    }
}

/// The desired release from `GET /v1/update`.
pub fn desired(update: &Value) -> Option<Previous> {
    let d = update.get("desired")?;
    Some(Previous {
        commit: d.get("commit")?.as_str()?.to_owned(),
        generation: d.get("generation")?.as_u64()?,
    })
}

fn frontend(update: &Value) -> Option<&Value> {
    update.get("frontend").filter(|f| f.is_object())
}

/// A frontend update is pending or activating.
pub fn frontend_in_flight(update: &Value) -> bool {
    frontend(update)
        .and_then(|f| f.get("state"))
        .and_then(Value::as_str)
        .is_some_and(|s| matches!(s, "pending" | "activating"))
}

/// The state of our frontend order; another order's state is not ours.
pub fn frontend_state(update: &Value, order_id: u64) -> Option<String> {
    let f = frontend(update)?;
    if f.get("order_id").and_then(Value::as_u64) != Some(order_id) {
        return None;
    }
    f.get("state").and_then(Value::as_str).map(str::to_owned)
}

/// Does `running` (12 hex digits or 40) name `commit`?
pub fn same_commit(running: &str, commit: &str) -> bool {
    running.len() >= 12 && commit.starts_with(running)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::platform_of;

    /// Proves a node's platform is the one it reports, else its OS and CPU in the store's
    /// spelling (`macos` is `darwin`), and a platform the store does not know is none.
    /// Catches a Mac node read as Linux, a node with half its software facts given a
    /// platform, and an unknown platform passed on to the preflight.
    ///
    /// Mutants planted: the `macos` rename dropped; the reported platform ignored; the
    /// filter on known platforms dropped.
    #[test]
    fn the_platform_is_reported_or_derived_and_must_be_known() {
        let of = |v: serde_json::Value| platform_of(&v);
        assert_eq!(
            of(
                json!({"update": {"platform": "linux-arm64"}, "software": {"os": "macos", "arch": "arm64"}})
            ),
            Some("linux-arm64".to_owned())
        );
        assert_eq!(
            of(json!({"software": {"os": "macos", "arch": "arm64"}})),
            Some("darwin-arm64".to_owned())
        );
        assert_eq!(
            of(json!({"software": {"os": "linux", "arch": "x86_64"}})),
            Some("linux-x86_64".to_owned())
        );
        assert_eq!(
            of(json!({"software": {"os": "linux", "arch": "riscv"}})),
            None
        );
        assert_eq!(of(json!({"software": {"os": "linux"}})), None);
        assert_eq!(of(json!({"software": {"arch": "arm64"}})), None);
        assert_eq!(of(json!({"update": {"platform": "beos-ppc"}})), None);
        assert_eq!(of(json!({})), None);
    }
}
