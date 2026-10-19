//! The builds the farm is running (gate A) and how their attempts ended (gate C), read from
//! `GET /v1/invocations` and `GET /v1/invocations/{id}/actions` (docs/api.md). Nothing here
//! trusts a reply: an id that could not be put in a path, a page that is not what the route
//! says, or a listing past [`MAX_PAGES`] is an error, and the gates read an error as red.

use serde_json::Value;

use crate::deploy::Run;

/// Pages read for one listing before it is called cut off.
const MAX_PAGES: usize = 50;

/// A server-given id may go into a path only if it is made of the characters of an
/// invocation id (hex, `~` for a collision suffix) and the usual id punctuation.
fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'~' | b'-' | b'_' | b'.'))
        && id != "."
        && id != ".."
}

/// A `next_page` link is followed only when it stays on the route that was asked for.
fn same_route(link: &str, prefix: &str) -> bool {
    link.starts_with(prefix) && !link.contains("..") && !link.contains("//")
}

/// Whether an attempt's failure class is one the farm is answerable for: `infra`, `lost`
/// and every `lost_*` (the classes of `invocation.proto` that are not the user's action
/// failing).
pub(super) fn is_farm_class(class: &str) -> bool {
    class == "infra" || class == "lost" || class.starts_with("lost_")
}

/// The ids of `GET /v1/invocations?{query}`, every page.
pub(super) fn list(run: &Run, query: &str) -> Result<Vec<String>, String> {
    let mut path = format!("/v1/invocations?{query}&page_size=200");
    let mut ids = Vec::new();
    for _ in 0..MAX_PAGES {
        let page = run.get_json(&path)?;
        let items = page
            .get("invocations")
            .and_then(Value::as_array)
            .ok_or("the listing has no `invocations` array")?;
        for item in items {
            let id = item
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| safe_id(id))
                .ok_or("a listed invocation has no usable id")?;
            ids.push(id.to_owned());
        }
        if page.get("partial").and_then(Value::as_bool) == Some(true) {
            return Err("the listing is partial".to_owned());
        }
        match page.get("next_page").and_then(Value::as_str) {
            None => return Ok(ids),
            Some(next) if same_route(next, "/v1/invocations?") => path = next.to_owned(),
            Some(_) => return Err("a next_page link leaves the listing".to_owned()),
        }
    }
    Err(format!("the listing is longer than {MAX_PAGES} pages"))
}

/// The attempts of invocation `id` that ended in a farm class, as `id: class`. An id the
/// server no longer holds is an error: nothing was proved about it.
pub(super) fn farm_class_failures(run: &Run, id: &str) -> Result<Vec<String>, String> {
    if !safe_id(id) {
        return Err(format!("`{id}` is not an invocation id"));
    }
    let prefix = format!("/v1/invocations/{id}/actions?");
    let mut path = format!("{prefix}page_size=1000");
    let mut found = Vec::new();
    for _ in 0..MAX_PAGES {
        let page = run
            .get_json(&path)
            .map_err(|e| format!("build {id}: {e}"))?;
        let rows = page
            .get("actions")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("build {id}: the page has no `actions` array"))?;
        for row in rows {
            let attempts = row
                .get("attempts")
                .and_then(Value::as_array)
                .into_iter()
                .flatten();
            for attempt in attempts {
                let class = attempt.pointer("/end/failure").and_then(Value::as_str);
                if let Some(class) = class.filter(|c| is_farm_class(c)) {
                    found.push(format!("{id}: {class}"));
                }
            }
        }
        match page.get("next_page").and_then(Value::as_str) {
            None => return Ok(found),
            Some(next) if same_route(next, &prefix) => path = next.to_owned(),
            Some(_) => return Err(format!("build {id}: a next_page link leaves the listing")),
        }
    }
    Err(format!(
        "build {id}: more than {MAX_PAGES} pages of actions"
    ))
}
