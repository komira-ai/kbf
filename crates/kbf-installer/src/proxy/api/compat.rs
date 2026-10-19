//! Rule R1 (`docs/design/frontend-proxy.md` section 9): the new frontend accepts every
//! protocol from its `min_protocol` up, so a release whose `min_protocol` is above the
//! oldest protocol a daemon or a storage member speaks would lock that node out. The gate
//! refuses such a release before any child starts.
//!
//! It is a pure function of the staged release's facts and a [`FleetSnapshot`]. It refuses
//! first when a read failed or the child is older than the protocol fields (the snapshot
//! says so), and then, for the daemons and after them the storage members, when:
//!
//! 1. the view lists a node that names no protocol (`protocol_unknown`), or lists nodes and
//!    names no oldest protocol at all (a combination the server does not send);
//! 2. a node is enrolled but the child has not seen it, so its protocol is unknown: a
//!    daemon that is offline, or one that has not dialed this child since a swap;
//! 3. `min_protocol` is above the view's `oldest_protocol`.
//!
//! A fleet with no node in it, enrolled or listed, is compatible: no one can be locked out.
//! The limits are the child's, and they make an empty fleet weaker than it sounds: the
//! child lists only the daemons that connected to it since it started, and a child with no
//! enrollment (a 404) enrolls no one. A freshly swapped child that no daemon has dialed
//! yet therefore reads as an empty fleet and passes, and a daemon with a hand-issued
//! certificate that is not connected now is not seen. Until enrollment is mounted or a
//! last-`Hello` protocol is stored per daemon, R1 protects the daemons already connected
//! to the child and the members in the store, no more. A node that connects between the
//! read and the switch is not seen either.

use super::fleet::FleetSnapshot;
use super::traits::{Compat, CompatGate, Staged};
use crate::CommitId;

/// The rule's name in `Compat::Incompatible`, the 409's `rule` field and `failure.reason`.
const RULE: &str = "R1";

/// The most names a refusal lists; the rest are counted.
const SHOWN: usize = 5;

/// The gate of the proxy: R1.
pub struct R1Gate;

impl CompatGate for R1Gate {
    fn check(&self, _commit: &CommitId, staged: Option<Staged<'_>>) -> Compat {
        // Before the release is fetched its `min_protocol` is not known.
        let Some(staged) = staged else {
            return Compat::Compatible;
        };
        match judge(staged.facts.min_protocol, staged.fleet) {
            Ok(()) => Compat::Compatible,
            Err(detail) => Compat::Incompatible {
                rule: RULE.to_owned(),
                detail,
            },
        }
    }
}

fn judge(min_protocol: u32, fleet: &FleetSnapshot) -> Result<(), String> {
    let nodes = fleet
        .nodes
        .as_ref()
        .map_err(|e| format!("the daemons could not be listed: {e}"))?;
    let members = fleet
        .members
        .as_ref()
        .map_err(|e| format!("the storage members could not be listed: {e}"))?;
    let enrolled = fleet
        .enrolled
        .as_ref()
        .map_err(|e| format!("the enrolled nodes could not be listed: {e}"))?;
    let sides = [
        ("daemons", nodes, &enrolled.daemons),
        ("storage members", members, &enrolled.storage),
    ];
    for (who, listed, expected) in sides {
        if listed.unknown > 0 {
            return Err(format!(
                "{} of the {who} name no protocol, so they cannot be judged",
                listed.unknown
            ));
        }
        if listed.oldest.is_none() && !listed.names.is_empty() {
            // Nodes listed, none counted unknown, and no oldest protocol: the server does
            // not send that, and `None` must not read as "every node is fine".
            return Err(format!(
                "the {who} are listed but the view names no oldest protocol, so they cannot be judged"
            ));
        }
        let missing: Vec<&str> = expected
            .difference(&listed.names)
            .map(String::as_str)
            .collect();
        if !missing.is_empty() {
            return Err(format!(
                "enrolled {who} the active child has not seen, so their protocol is unknown: {}",
                list(&missing)
            ));
        }
        if let Some(oldest) = listed.oldest
            && min_protocol > oldest
        {
            return Err(format!(
                "the release's min_protocol {min_protocol} is above the oldest protocol of the {who}, {oldest}"
            ));
        }
    }
    Ok(())
}

/// The first [`SHOWN`] names, and how many more there are.
fn list(names: &[&str]) -> String {
    let shown = names
        .iter()
        .take(SHOWN)
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
    match names.len().saturating_sub(SHOWN) {
        0 => shown,
        more => format!("{shown} and {more} more"),
    }
}
