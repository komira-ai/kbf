//! Reads run records through the operator API's history routes and turns them into rows.
//!
//! The walk, all `GET`:
//! 1. `/v1/nodes` and `/v1/nodes/{id}`: each node's OS class and capacity, as they are now;
//! 2. `/v1/invocations?since=&until=` (pages): the builds of the window;
//! 3. `/v1/invocations/{id}/actions` (pages): each action's attempts and their run ids
//!    (the server must run with `--history-objects`; a build with no run ids yields no
//!    rows and is counted);
//! 4. `/v1/runs/{run_id}`: one attempt, rendered from its run record.
//!
//! Limits, all inherited from the read API: only builds the history index lists are seen,
//! and only attempts with a run record (calls with no invocation id leave none); a build's
//! actions are read from its record, which is in memory for `--invocation-memory` or in
//! history for the retention class; node capacity is a point in time; the API writes 0 for
//! an unmeasured peak or wall, which the row records as absent.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::readonly::{CallError, ReadOnly, Transport};
use crate::row::{Capacity, Row, Usage};

/// Entries asked for per page (the server's largest).
const PAGE: usize = 1000;
/// Pages followed for one list before the walk gives up (a server that never ends a list).
const MAX_PAGES: usize = 100_000;

/// Why an export stopped.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ExportError {
    /// A call failed.
    #[error(transparent)]
    Call(#[from] CallError),
    /// The server answered with a status the walk does not expect.
    #[error("GET {path}: status {status}: {body}")]
    Status {
        /// The path asked.
        path: String,
        /// The status.
        status: u16,
        /// The start of the body.
        body: String,
    },
    /// An answer is not the shape the walk reads.
    #[error("GET {path}: {why}")]
    Shape {
        /// The path asked.
        path: String,
        /// What is wrong.
        why: String,
    },
    /// A list never ended.
    #[error("GET {path}: more than {MAX_PAGES} pages")]
    Endless {
        /// The list's first path.
        path: String,
    },
    /// The window is not two RFC 3339 times in order.
    #[error("window: {0}")]
    Window(String),
}

/// The attempts to export: queued in `[since, until)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    since_us: u64,
    until_us: u64,
}

impl Window {
    /// The window between two RFC 3339 times.
    ///
    /// # Errors
    /// A time does not parse, or `until` is not after `since`.
    pub fn parse(since: &str, until: &str) -> Result<Self, ExportError> {
        let since_us = micros(since).map_err(ExportError::Window)?;
        let until_us = micros(until).map_err(ExportError::Window)?;
        if until_us <= since_us {
            return Err(ExportError::Window(format!("{until} is not after {since}")));
        }
        Ok(Self { since_us, until_us })
    }

    fn holds(self, us: u64) -> bool {
        (self.since_us..self.until_us).contains(&us)
    }

    fn query(self) -> String {
        let at = |us: u64| {
            let nanos = i128::from(us) * 1_000;
            OffsetDateTime::from_unix_timestamp_nanos(nanos)
                .ok()
                .and_then(|t| t.format(&Rfc3339).ok())
                .unwrap_or_default()
        };
        format!(
            "since={}&until={}",
            encode(&at(self.since_us)),
            encode(&at(self.until_us))
        )
    }
}

/// What the walk saw.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Builds listed in the window.
    pub invocations: u64,
    /// Attempts the builds' action rows hold.
    pub attempts: u64,
    /// Attempts with no run id (no run record, or the server runs without history objects).
    pub without_run_id: u64,
    /// Run ids two builds both listed, read once.
    pub duplicate_run_ids: u64,
    /// Run ids the run route does not hold (`404`: aged out of history).
    pub run_gone: u64,
    /// Runs read but queued outside the window.
    pub outside_window: u64,
    /// Rows exported.
    pub rows: u64,
}

/// The rows of a window and what the walk saw.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Export {
    /// Rows by queue time, then run id.
    pub rows: Vec<Row>,
    /// Counts.
    pub report: Report,
}

struct Node {
    platform: String,
    capacity: Option<Capacity>,
}

/// Reads every attempt of `window`.
///
/// # Errors
/// See [`ExportError`]. A run the server no longer holds is counted, not an error.
pub fn export<T: Transport>(api: &ReadOnly<T>, window: Window) -> Result<Export, ExportError> {
    let nodes = nodes(api)?;
    let mut report = Report::default();
    let list = format!("/v1/invocations?{}&page_size={PAGE}", window.query());
    let mut run_ids = BTreeSet::new();
    for build in pages(api, &list, "invocations")? {
        report.invocations += 1;
        let id = str_of(&build, "id").ok_or_else(|| shape(&list, "an invocation has no id"))?;
        let actions = format!("/v1/invocations/{}/actions?page_size={PAGE}", encode(id));
        for action in pages(api, &actions, "actions")? {
            for attempt in action["attempts"].as_array().into_iter().flatten() {
                report.attempts += 1;
                match str_of(attempt, "run_id") {
                    None => report.without_run_id += 1,
                    Some(run) => {
                        report.duplicate_run_ids += u64::from(!run_ids.insert(run.to_owned()));
                    }
                }
            }
        }
    }
    let mut rows = Vec::new();
    for run in &run_ids {
        let path = format!("/v1/runs/{}", encode(run));
        let Some(value) = get_json(api, &path)? else {
            report.run_gone += 1;
            continue;
        };
        let row = row_of(&value, &nodes).map_err(|why| shape(&path, &why))?;
        if window.holds(row.queued_us) {
            rows.push(row);
        } else {
            report.outside_window += 1;
        }
    }
    rows.sort_by(|a, b| (a.queued_us, &a.run_id).cmp(&(b.queued_us, &b.run_id)));
    report.rows = rows.len() as u64;
    Ok(Export { rows, report })
}

/// Every entry under `key` of the pages that start at `first`, following `next_page`.
fn pages<T: Transport>(
    api: &ReadOnly<T>,
    first: &str,
    key: &str,
) -> Result<Vec<Value>, ExportError> {
    let mut out = Vec::new();
    let mut path = first.to_owned();
    for _ in 0..MAX_PAGES {
        let page = get_json(api, &path)?.ok_or_else(|| shape(&path, "no such list (404)"))?;
        let entries = page[key]
            .as_array()
            .ok_or_else(|| shape(&path, &format!("no `{key}` array")))?;
        out.extend(entries.iter().cloned());
        match str_of(&page, "next_page") {
            Some(next) if is_api_path(next) => next.clone_into(&mut path),
            Some(_) => return Err(shape(&path, "next_page is not a plain /v1/ path")),
            None => return Ok(out),
        }
    }
    Err(ExportError::Endless {
        path: first.to_owned(),
    })
}

/// Whether a server-supplied `next_page` is safe to put in a request line: an API path with
/// no control character (a `\r\n` there would start another request).
fn is_api_path(next: &str) -> bool {
    next.starts_with("/v1/") && !next.chars().any(|c| c.is_control() || c == ' ')
}

fn nodes<T: Transport>(api: &ReadOnly<T>) -> Result<BTreeMap<String, Node>, ExportError> {
    let list = get_json(api, "/v1/nodes")?.ok_or_else(|| shape("/v1/nodes", "404"))?;
    let mut out = BTreeMap::new();
    for node in list["nodes"].as_array().into_iter().flatten() {
        let id = str_of(node, "node_id").ok_or_else(|| shape("/v1/nodes", "a node has no id"))?;
        let platform = match node["software"]["os_name"].as_str() {
            None | Some("") => "-",
            Some("macOS") => "macos",
            Some(_) => "linux",
        };
        let detail = get_json(api, &format!("/v1/nodes/{}", encode(id)))?;
        let capacity = detail.and_then(|d| {
            Some(Capacity {
                cpu_millis: d["capacity"]["cpu_millis"].as_u64()?,
                memory_bytes: d["capacity"]["memory_bytes"].as_u64()?,
            })
        });
        out.insert(
            id.to_owned(),
            Node {
                platform: platform.to_owned(),
                capacity,
            },
        );
    }
    Ok(out)
}

/// `GET path` as JSON: `None` for a 404, an error for any other non-2xx.
fn get_json<T: Transport>(api: &ReadOnly<T>, path: &str) -> Result<Option<Value>, ExportError> {
    let reply = api.get(path)?;
    if reply.status == 404 {
        return Ok(None);
    }
    if !(200..300).contains(&reply.status) {
        return Err(ExportError::Status {
            path: path.to_owned(),
            status: reply.status,
            body: reply.body.chars().take(200).collect(),
        });
    }
    serde_json::from_str(&reply.body)
        .map(Some)
        .map_err(|e| shape(path, &format!("not JSON: {e}")))
}

fn row_of(v: &Value, nodes: &BTreeMap<String, Node>) -> Result<Row, String> {
    let need = |key: &str| str_of(v, key).ok_or_else(|| format!("no `{key}`"));
    let node = str_of(v, "worker").unwrap_or_default();
    let known = nodes.get(node);
    let time = |key: &str| str_of(v, key).map(micros).transpose();
    let queued_us = micros(need("queued")?)?;
    let usage = v.get("usage").filter(|u| u.is_object());
    let shape_key = |key: &str| v["shape"][key].as_str().unwrap_or("-").to_owned();
    Ok(Row {
        run_id: need("run_id")?.to_owned(),
        instance: need("instance")?.to_owned(),
        platform: known.map_or("-", |n| n.platform.as_str()).to_owned(),
        node: node.to_owned(),
        shape: shape_key("key"),
        cfg: shape_key("cfg"),
        digest: str_of(v, "action").unwrap_or("-").to_owned(),
        outcome: need("class")?.to_owned(),
        memory_kill: str_of(v, "memory_kill").unwrap_or("none").to_owned(),
        booked_memory_bytes: v["booked_memory_bytes"].as_u64(),
        raised_memory_bytes: v["raised_memory_bytes"].as_u64(),
        memory_used_bytes: v["memory_used_bytes"].as_u64(),
        cgroup: usage.map(usage_of),
        queued_us,
        started_us: time("started")?,
        ended_us: time("ended")?,
        node_capacity: known.and_then(|n| n.capacity),
        basis: usage.and_then(|u| str_of(u, "basis")).map(str::to_owned),
        // The run route writes the operation id as a number (`operation_id`), left out for a
        // run with none. The text is its decimal form. No server wrote another key: the
        // exporter never read the run's operation, so no export file holds one to keep.
        operation: v["operation_id"].as_u64().map(|id| id.to_string()),
        // The run's own key is the lease's grant, written for every attempt; the usage block's
        // is what the daemon reported, which an older server wrote for a Mac only.
        booked_millicpus: v["booked_millicpus"]
            .as_u64()
            .or_else(|| usage.and_then(|u| u["booked_millicpus"].as_u64())),
        node_cpu_pressure_centi: usage.and_then(|u| u["node_cpu_pressure_centi"].as_u64()),
    })
}

/// The cgroup block. The read API writes 0 for a wall or peak that was not measured, so
/// 0 is read as absent for those two.
fn usage_of(u: &Value) -> Usage {
    let number = |key: &str| u[key].as_u64();
    let measured = |key: &str| number(key).filter(|n| *n > 0);
    Usage {
        cpu_usage_us: number("cpu_usage_us").unwrap_or(0),
        wall_us: measured("wall_us"),
        memory_peak_bytes: measured("memory_peak_bytes"),
        memory_events_max: number("memory_events_max"),
        memory_events_oom_kill: number("memory_events_oom_kill"),
        memory_swap_peak_bytes: number("memory_swap_peak_bytes"),
        memory_anon_swap_peak_bytes: number("memory_anon_swap_peak_bytes"),
        cpu_pressure_some_us: number("cpu_pressure_some_us"),
    }
}

fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key)?.as_str()
}

fn shape(path: &str, why: &str) -> ExportError {
    ExportError::Shape {
        path: path.to_owned(),
        why: why.to_owned(),
    }
}

/// An RFC 3339 time in microseconds after the epoch.
fn micros(text: &str) -> Result<u64, String> {
    let t = OffsetDateTime::parse(text, &Rfc3339).map_err(|e| format!("{text:?}: {e}"))?;
    u64::try_from(t.unix_timestamp_nanos() / 1_000)
        .map_err(|_| format!("{text:?}: before the epoch"))
}

/// A path segment or query value, percent-encoded.
fn encode(text: &str) -> String {
    let mut out = String::new();
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}
