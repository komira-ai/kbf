//! The read API's JSON documents, as the pages read them: the keys the `/v1` routes
//! answer (`docs/api.md`, `docs/design/ui-data.md` 6.3), never the server's internal
//! structs. Only the keys a page shows are named; serde ignores the rest, so a key the
//! server adds changes no page, and a key a page names that the server stops sending
//! makes that read fail as malformed rather than render a guess. `kbf-server`'s
//! `tests/ui_pages.rs` parses these from the real routes.

use serde::{Deserialize, Deserializer, de};
use serde_json::Value;

/// `GET /v1/invocations/{id}`.
#[derive(Debug, Deserialize)]
pub(crate) struct Invocation {
    pub(crate) id: String,
    pub(crate) tool: Option<String>,
    pub(crate) tool_version: Option<String>,
    pub(crate) instances: Vec<String>,
    pub(crate) first_seen: String,
    pub(crate) last_seen: String,
    pub(crate) state: String,
    pub(crate) complete: bool,
    pub(crate) rows_capped: bool,
    pub(crate) actions: u64,
    pub(crate) counts: Counts,
}

/// The invocation's counters.
#[derive(Debug, Deserialize)]
pub(crate) struct Counts {
    pub(crate) execute: u64,
    pub(crate) cache_hit_execute: u64,
    pub(crate) joined: u64,
    pub(crate) ran: u64,
    pub(crate) failed: u64,
    pub(crate) ac_hit: u64,
    pub(crate) ac_miss: u64,
    pub(crate) bytes_uploaded: u64,
    pub(crate) bytes_downloaded: u64,
}

/// A page of `GET /v1/invocations/{id}/actions`.
#[derive(Debug, Deserialize)]
pub(crate) struct ActionsPage {
    pub(crate) actions: Vec<Row>,
    pub(crate) next_page_token: Option<String>,
}

/// One action row, as `/actions` lists it.
#[derive(Debug, Deserialize)]
pub(crate) struct Row {
    pub(crate) action: String,
    pub(crate) instance: String,
    pub(crate) target: Option<Text>,
    pub(crate) mnemonic: Option<Text>,
    pub(crate) how: String,
    pub(crate) attempts: Vec<Attempt>,
    #[serde(default, deserialize_with = "known_or_other")]
    pub(crate) outcome: Option<Outcome>,
}

/// A client-sent string, cut at the server's bound.
#[derive(Debug, Deserialize)]
pub(crate) struct Text {
    pub(crate) value: String,
    pub(crate) truncated: bool,
}

/// One attempt at running an action.
#[derive(Debug, Deserialize)]
pub(crate) struct Attempt {
    pub(crate) lease: String,
    pub(crate) worker: String,
    pub(crate) queued: String,
    pub(crate) started: Option<String>,
    pub(crate) ended: Option<String>,
    pub(crate) end: Option<End>,
}

/// How an attempt ended: `{"result": "ok"}`, `{"requeue": .., "reason": ..}` or
/// `{"failure": .., "reason": ..}`.
#[derive(Debug, Deserialize)]
pub(crate) struct End {
    pub(crate) result: Option<String>,
    pub(crate) requeue: Option<String>,
    pub(crate) failure: Option<String>,
    pub(crate) reason: Option<String>,
}

/// The action's outcome, as the client was answered: one tag, with its fields.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Outcome {
    Ok {
        exit_code: i64,
    },
    Failed {
        code: i64,
        message: String,
    },
    TimedOut {},
    Refused {
        reason: String,
    },
    /// A tag this page does not know, by its tag: the server adds outcomes
    /// (`docs/design/ui-data.md` 6.1), and a page shows a new one rather than fail.
    #[serde(skip)]
    Other(String),
}

/// The tags of [`Outcome`] this page reads; any other is [`Tag::Unknown`].
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Tag {
    Ok,
    Failed,
    TimedOut,
    Refused,
    #[serde(other)]
    Unknown,
}

/// An outcome: an object of one tag this page does not know is [`Outcome::Other`];
/// anything else is read as [`Outcome`], so a known tag of another shape, or an
/// outcome that is not one tag, is malformed.
fn known_or_other<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Outcome>, D::Error> {
    let Some(v) = Option::<Value>::deserialize(d)? else {
        return Ok(None);
    };
    let tag = v
        .as_object()
        .filter(|m| m.len() == 1)
        .and_then(|m| m.keys().next());
    if let Some(tag) = tag {
        let known = serde_json::from_value(Value::String(tag.clone())).unwrap_or(Tag::Unknown);
        if matches!(known, Tag::Unknown) {
            return Ok(Some(Outcome::Other(tag.clone())));
        }
    }
    Outcome::deserialize(v).map(Some).map_err(de::Error::custom)
}

/// `GET /v1/invocations/{id}/actions/{digest}`: the row with its listing.
#[derive(Debug, Deserialize)]
pub(crate) struct Action {
    #[serde(flatten)]
    pub(crate) row: Row,
    pub(crate) command: Option<String>,
    pub(crate) outputs: Vec<Output>,
    pub(crate) outputs_truncated: bool,
    pub(crate) stdout: Option<Blob>,
    pub(crate) stderr: Option<Blob>,
    pub(crate) usage: Option<Usage>,
}

/// A blob the answer links to.
#[derive(Debug, Deserialize)]
pub(crate) struct Blob {
    pub(crate) digest: String,
    pub(crate) available: Option<bool>,
    pub(crate) href: Option<String>,
}

impl Blob {
    /// The size the digest names (`<sha256 hex>-<size>`), if it parses.
    pub(crate) fn size_bytes(&self) -> Option<u64> {
        self.digest.rsplit_once('-')?.1.parse().ok()
    }
}

/// One output of an action.
#[derive(Debug, Deserialize)]
pub(crate) struct Output {
    pub(crate) path: String,
    #[serde(flatten)]
    pub(crate) blob: Blob,
    pub(crate) directory: bool,
}

/// What the action used, as its worker measured it.
#[derive(Debug, Deserialize)]
pub(crate) struct Usage {
    pub(crate) cpu_user_micros: u64,
    pub(crate) cpu_system_micros: u64,
    pub(crate) peak_memory_bytes: u64,
    pub(crate) wall_micros: u64,
}

/// `GET /v1/update`. `policy` stays a raw value: whether it has a `paused` key at all
/// decides what the page says about a pause ([`UpdateDoc::pause`]).
#[derive(Debug, Deserialize)]
pub(crate) struct UpdateDoc {
    pub(crate) desired: Option<Desired>,
    /// The server's own flag: the policy's `paused`, false without a policy row.
    #[serde(default)]
    pub(crate) paused: Option<bool>,
    pub(crate) policy: Option<Value>,
    pub(crate) releases: Releases,
}

/// The desired release.
#[derive(Debug, Deserialize)]
pub(crate) struct Desired {
    pub(crate) generation: u64,
    pub(crate) commit: String,
    pub(crate) set_at_ms: u64,
    /// The offer said it is a rollback (a release other than the frontend's own). A server
    /// that predates the field does not send it: false.
    #[serde(default)]
    pub(crate) rollback: bool,
}

/// The body of `GET /v1/releases`, as `GET /v1/update` embeds it.
#[derive(Debug, Deserialize)]
pub(crate) struct Releases {
    pub(crate) releases: Vec<Release>,
}

/// One release: a commit and what the store holds of it per platform.
#[derive(Debug, Deserialize)]
pub(crate) struct Release {
    pub(crate) commit: String,
    pub(crate) platforms: Vec<ReleasePlatform>,
}

/// One platform of a release.
#[derive(Debug, Deserialize)]
pub(crate) struct ReleasePlatform {
    pub(crate) platform: String,
    pub(crate) complete: bool,
}

/// Whether the farm is paused, as the page reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Pause {
    Paused,
    Running,
    /// Neither the policy nor the desired release says: never shown as running.
    Unknown,
}

impl UpdateDoc {
    /// `policy.paused` only when the policy object has that key (a value that is not a
    /// boolean is unknown), else the document's own `paused`; unknown when neither is
    /// there. The desired row carries no pause: it lives in the policy row. A
    /// policy row without the key never reads as running.
    pub(crate) fn pause(&self) -> Pause {
        let from_policy = self
            .policy
            .as_ref()
            .and_then(|p| p.get("paused"))
            .map(Value::as_bool);
        let paused = match from_policy {
            Some(paused) => paused,
            None => self.paused,
        };
        match paused {
            Some(true) => Pause::Paused,
            Some(false) => Pause::Running,
            None => Pause::Unknown,
        }
    }

    /// A boolean of the policy object, if the policy has it.
    pub(crate) fn policy_flag(&self, key: &str) -> Option<bool> {
        self.policy.as_ref()?.get(key)?.as_bool()
    }
}

/// `GET /v1/nodes`.
#[derive(Debug, Deserialize)]
pub(crate) struct NodesDoc {
    pub(crate) server: ServerInfo,
    pub(crate) nodes: Vec<Node>,
}

/// The server that answers.
#[derive(Debug, Deserialize)]
pub(crate) struct ServerInfo {
    pub(crate) commit: String,
}

/// One node of `GET /v1/nodes`.
#[derive(Debug, Deserialize)]
pub(crate) struct Node {
    pub(crate) node_id: String,
    pub(crate) software: Option<Software>,
    pub(crate) update: UpdateSummary,
    /// Not served yet; shown when a server sends it.
    #[serde(default)]
    pub(crate) role: Option<String>,
}

/// What a node says about its software; the page reads its OS.
#[derive(Debug, Deserialize)]
pub(crate) struct Software {
    pub(crate) os_name: String,
}

/// Where a node's update is, the open summary.
#[derive(Debug, Deserialize)]
pub(crate) struct UpdateSummary {
    pub(crate) state: Option<String>,
    pub(crate) up_to_date: Option<bool>,
    pub(crate) availability: String,
}

/// `GET /v1/update/hosts`.
#[derive(Debug, Deserialize)]
pub(crate) struct HostsDoc {
    pub(crate) hosts: Vec<Host>,
    pub(crate) next_page_token: Option<String>,
}

/// One host's detail.
#[derive(Debug, Deserialize)]
pub(crate) struct Host {
    pub(crate) node_id: String,
    pub(crate) desired_commit: Option<String>,
    pub(crate) running_commit: Option<String>,
    pub(crate) refusal: Option<String>,
}
