//! `status.json`: the helper's word to the node, written whole each time it changes, and
//! world-readable (the node runs as the service user). The schema is the one
//! `kbf-update`'s client reads (`helper.rs`).

use crate::CommitId;
use crate::frontend::pending::{End, FrontendFile, Phase};
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

/// The frontend's own update, apart from the node fields (each kind has its own floor):
/// the newest `frontend_update` order, by the id the frontend gave it, and where it stands.
/// Absent until this host has acted on one, and on an installer that predates it, so a
/// reader treats a missing section as "no word", never as an outcome.
///
/// `state`: `pending` (in flight), `needs_operator` (an operator must look; `refusal` says
/// why), then the terminal words `confirmed`, `reverted`, `refused`.
#[derive(Serialize)]
struct Frontend<'a> {
    order_id: u64,
    epoch: u64,
    commit: &'a CommitId,
    state: &'static str,
    #[serde(skip_serializing_if = "str::is_empty")]
    refusal: &'a str,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    frontend: Option<Frontend<'a>>,
}

/// The frontend section: the update in flight if there is one, else how the last ended.
fn frontend_section(file: &FrontendFile) -> Option<Frontend<'_>> {
    if let Some(p) = &file.frontend_pending {
        let (state, refusal) = match &p.phase {
            Phase::NeedsOperator { reason } => ("needs_operator", reason.as_str()),
            _ => ("pending", ""),
        };
        return Some(Frontend {
            order_id: p.order_id,
            epoch: p.epoch,
            commit: &p.commit,
            state,
            refusal,
        });
    }
    file.last.as_ref().map(|e| Frontend {
        order_id: e.order_id,
        epoch: e.epoch,
        commit: &e.commit,
        state: match e.end {
            End::Confirmed => "confirmed",
            End::Reverted => "reverted",
            End::Refused => "refused",
        },
        refusal: &e.why,
    })
}

/// The word for what the host is doing. A swap on trial outranks everything; then a
/// refusal or a revert the operator has not yet moved past; then a staged release.
fn word(state: &State, memory: &Memory) -> &'static str {
    if memory.fetching {
        "fetching"
    } else if state.pending_confirm.is_some() | memory.frontend_pending {
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
pub fn render(state: &State, memory: &Memory, frontend: Option<&FrontendFile>) -> Vec<u8> {
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
        frontend: frontend.and_then(frontend_section),
    };
    let mut bytes = serde_json::to_vec(&file).expect("plain fields serialize");
    bytes.push(b'\n');
    bytes
}
