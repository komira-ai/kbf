//! `status.json`: the helper's word to the node, written whole each time it changes, and
//! world-readable (the node runs as the service user). The schema is the one
//! `kbf-update`'s client reads (`helper.rs`).

use crate::CommitId;
use crate::state::State;
use serde::Serialize;

use crate::helper::memory::{Event, Memory};

/// The schema number the node reads.
pub const SCHEMA: u64 = 2;

#[derive(Serialize)]
struct Staged {
    protocol: u32,
    min_protocol: u32,
    state_schema: u32,
}

/// The pinned Envoy, once the installer manages one (a later change); today neither is set.
#[derive(Serialize)]
struct Envoy {
    current: Option<String>,
    previous: Option<String>,
}

#[derive(Serialize)]
struct File<'a> {
    schema: u64,
    installed_commit: Option<&'a CommitId>,
    previous_commit: Option<&'a CommitId>,
    staged_commit: Option<&'a CommitId>,
    state: &'static str,
    refusal: &'a str,
    acked_order_id: u64,
    staged: Option<Staged>,
    envoy: Envoy,
    /// Releases a link names that carry no `VERIFIED` marker (adopted by `migrate`); left out
    /// when there are none, so a status of a host that has none reads as it always did.
    #[serde(skip_serializing_if = "<[CommitId]>::is_empty")]
    unverified: &'a [CommitId],
}

/// The word for what the host is doing. A swap on trial outranks everything; then a
/// refusal or a revert the operator has not yet moved past; then a staged release.
fn word(state: &State, memory: &Memory) -> &'static str {
    if memory.fetching {
        "fetching"
    } else if state.pending_confirm.is_some() {
        "activated_unconfirmed"
    } else if memory.event == Event::Refused {
        "refused"
    } else if (memory.event == Event::Reverted) & state.staged.is_none() {
        "reverted"
    } else if state.staged.is_some() {
        "staged"
    } else if memory.event == Event::Confirmed {
        "confirmed"
    } else {
        "idle"
    }
}

/// The bytes of `status.json` for a host in `state` that remembers `memory`. The facts of
/// the newest fetched release are shown while it is staged or current.
pub fn render(state: &State, memory: &Memory) -> Vec<u8> {
    render_with(state, memory, &[])
}

/// The same, naming the releases `current` or `previous` hold that were adopted without
/// verification, so that nothing reading the status takes them for verified.
pub fn render_with(state: &State, memory: &Memory, unverified: &[CommitId]) -> Vec<u8> {
    let staged = memory
        .staged
        .as_ref()
        .filter(|f| {
            state.staged.as_ref() == Some(&f.commit) || state.current.as_ref() == Some(&f.commit)
        })
        .map(|f| Staged {
            protocol: f.protocol,
            min_protocol: f.min_protocol,
            state_schema: f.state_schema,
        });
    let file = File {
        schema: SCHEMA,
        installed_commit: state.current.as_ref(),
        previous_commit: state.previous.as_ref(),
        staged_commit: state.staged.as_ref(),
        state: word(state, memory),
        refusal: &memory.refusal,
        acked_order_id: memory.acked_order_id,
        staged,
        envoy: Envoy {
            current: None,
            previous: None,
        },
        unverified,
    };
    let mut bytes = serde_json::to_vec(&file).expect("plain fields serialize");
    bytes.push(b'\n');
    bytes
}
