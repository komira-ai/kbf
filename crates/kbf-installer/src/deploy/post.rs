//! Steps 5 and 7: wait for every node to confirm, then check the farm.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use super::api::Request;
use super::nodes::{self, Node};
use super::summary::{Exit, NodeResult};
use super::{Fail, Run, SmokeResult, fail};

impl Run<'_> {
    /// The generation under which the new frontend offered the commit. The frontend makes
    /// the offer in the `nodes` phase of its update, so it is there once the update is
    /// `done`; it is read until it is (a read can fail while the children settle) for the
    /// budget of one node. The deploy does not make the offer itself: if the desired release
    /// is not the commit, nothing rolls the nodes and the deploy says so.
    fn await_offer(&mut self) -> Result<u64, Fail> {
        let start = self.env.clock.now();
        let commit = self.opts.commit.as_str();
        let seen = loop {
            let seen = match self.get_json("/v1/update") {
                Ok(update) => match nodes::desired(&update) {
                    Some(offer) if offer.commit == commit => {
                        self.sum.generation = Some(offer.generation);
                        return Ok(offer.generation);
                    }
                    Some(offer) => format!("the desired release is {}", offer.commit),
                    None => "no desired release is set".to_owned(),
                },
                Err(e) => e,
            };
            let waited = self.env.clock.now().saturating_sub(start);
            if waited >= self.opts.node_timeout.as_secs() {
                break seen;
            }
            self.env.clock.sleep(self.opts.poll);
        };
        Err(fail(
            Exit::Failed,
            "nodes_not_offered",
            format!("the new frontend did not offer the commit to the nodes: {seen}"),
        ))
    }

    /// Step 5. The clock for a node starts when the previous one confirms (or at the
    /// start), so a slow storage node does not share one budget with the rest. The first
    /// node that does not confirm ends the deploy; no later node is waited for, and none
    /// is ordered: the new frontend's controller owns the roll. Every node the preflight
    /// saw is waited for, listed or not: the new frontend lists only the nodes that have
    /// reconnected to it.
    pub(super) fn wait_nodes(&mut self) -> Result<(), Fail> {
        let commit = self.opts.commit.to_string();
        let generation = self.await_offer()?;
        let mut done: BTreeMap<String, NodeResult> = BTreeMap::new();
        let mut last = self.env.clock.now();
        loop {
            let seen = self.get_json("/v1/nodes").ok().map(|v| nodes::list(&v));
            let now = self.env.clock.now();
            if let Some(list) = &seen {
                for node in list {
                    if done.contains_key(&node.id) {
                        continue;
                    }
                    if !node.connected && self.opts.allow_offline.contains(&node.id) {
                        done.insert(node.id.clone(), result(node, "offline", 0));
                        continue;
                    }
                    if node.state == "confirmed"
                        && nodes::same_commit(&node.running_commit, &commit)
                    {
                        done.insert(
                            node.id.clone(),
                            result(node, "confirmed", now.saturating_sub(last)),
                        );
                        last = now;
                        continue;
                    }
                    // A state left from an older offer is not this deploy's.
                    let current = node.offered_generation.is_none_or(|g| g >= generation);
                    let verdict = if !current {
                        None
                    } else if node.state == "reverted" {
                        Some("reverted")
                    } else if !node.refusal.is_empty() {
                        Some("refused")
                    } else if node.availability == "unsupported" {
                        Some("unsupported")
                    } else {
                        None
                    };
                    if let Some(state) = verdict {
                        done.insert(
                            node.id.clone(),
                            result(node, state, now.saturating_sub(last)),
                        );
                        return Err(self.stop(list, done, &node.id, state));
                    }
                }
                // A node that may be offline and is not listed is as good as one that is.
                for id in self.expected.keys() {
                    if self.opts.allow_offline.contains(id) && !list.iter().any(|n| &n.id == id) {
                        done.entry(id.clone())
                            .or_insert_with(|| self.node_result(list, id, "offline", 0));
                    }
                }
                // An empty list confirms nothing.
                if !list.is_empty() && self.nodes(list).all(|id| done.contains_key(id)) {
                    self.sum.nodes = done.into_values().collect();
                    return Ok(());
                }
            }
            if now.saturating_sub(last) >= self.opts.node_timeout.as_secs() {
                let list = seen.unwrap_or_default();
                // The first node, in id order, that is still waited for. None when every node
                // may be offline and none is listed: an empty list confirms nothing.
                let stuck = self.nodes(&list).find(|id| !done.contains_key(*id));
                let id = stuck.unwrap_or("no node listed").to_owned();
                if stuck.is_some() {
                    let late = self.node_result(&list, &id, "timed_out", now.saturating_sub(last));
                    done.insert(id.clone(), late);
                }
                return Err(self.stop(&list, done, &id, "timed_out"));
            }
            self.env.clock.sleep(self.opts.poll);
        }
    }

    /// Every node the deploy waits for, in id order: those the preflight saw and those
    /// `list` has (a node that enrolled since).
    fn nodes<'n>(&'n self, list: &'n [Node]) -> impl Iterator<Item = &'n str> {
        let ids: BTreeSet<&str> = self
            .expected
            .keys()
            .map(String::as_str)
            .chain(list.iter().map(|n| n.id.as_str()))
            .collect();
        ids.into_iter()
    }

    /// The result of node `id`: from `list` when it is there, else from the preflight.
    fn node_result(&self, list: &[Node], id: &str, state: &str, secs: u64) -> NodeResult {
        match list.iter().find(|n| n.id == id) {
            Some(n) => result(n, state, secs),
            None => NodeResult {
                platform: self.expected.get(id).cloned().unwrap_or_default(),
                secs,
                ..NodeResult::new(id, state)
            },
        }
    }

    /// Record every node (those not reached are `pending`) and build the failure.
    fn stop(
        &mut self,
        list: &[Node],
        mut done: BTreeMap<String, NodeResult>,
        id: &str,
        state: &str,
    ) -> Fail {
        for waited in self.nodes(list) {
            done.entry(waited.to_owned())
                .or_insert_with(|| self.node_result(list, waited, "pending", 0));
        }
        self.sum.nodes = done.into_values().collect();
        fail(
            Exit::Failed,
            "node_not_confirmed",
            format!("{id}: {state}; the farm is left mixed and the host held"),
        )
    }

    /// Step 7. Every check runs and is recorded, even after one fails.
    pub(super) fn post_checks(&mut self) -> Result<SmokeResult, Fail> {
        let commit = self.opts.commit.to_string();
        let mut good = true;

        let start = self.env.clock.now();
        let members = loop {
            let ok = self
                .get_json("/v1/members")
                .ok()
                .is_some_and(|m| all_active(&m));
            if ok
                || self.env.clock.now().saturating_sub(start) >= self.opts.members_timeout.as_secs()
            {
                break ok;
            }
            self.env.clock.sleep(self.opts.poll);
        };
        self.sum.post.members_active = Some(members);

        let on_commit = self
            .get_json("/v1/nodes")
            .ok()
            .is_some_and(|v| nodes_on_commit(&v, &commit));
        self.sum.post.nodes_on_commit = Some(on_commit);

        let ready = self
            .send(&Request::get("/readyz"))
            .is_ok_and(|r| r.status == 200);
        self.sum.post.readyz = Some(ready);

        let smoke = self.env.smoke.run();
        self.sum.post.cas_smoke = Some(
            match &smoke {
                SmokeResult::Pass => "pass",
                SmokeResult::Fail(_) => "fail",
                SmokeResult::Skipped => "skipped",
            }
            .to_owned(),
        );
        good &= members && on_commit && ready && !matches!(smoke, SmokeResult::Fail(_));
        if good {
            Ok(smoke)
        } else {
            let detail = match smoke {
                SmokeResult::Fail(why) => format!("the CAS smoke failed: {why}"),
                _ => String::new(),
            };
            Err(fail(Exit::Failed, "postcheck_failed", detail))
        }
    }
}

fn result(n: &Node, state: &str, secs: u64) -> NodeResult {
    NodeResult {
        id: n.id.clone(),
        platform: n.platform.clone().unwrap_or_default(),
        state: state.to_owned(),
        running_commit: n.running_commit.clone(),
        secs,
        refusal: n.refusal.clone(),
    }
}

/// Every member `active`, and at least one member.
fn all_active(members: &Value) -> bool {
    members
        .get("members")
        .and_then(Value::as_array)
        .is_some_and(|ms| {
            !ms.is_empty()
                && ms
                    .iter()
                    .all(|m| m.get("state").and_then(Value::as_str) == Some("active"))
        })
}

/// At least one node, and every node connected, serving, up to date and on the commit; the server on it too.
pub(super) fn nodes_on_commit(v: &Value, commit: &str) -> bool {
    let server = v
        .pointer("/server/commit")
        .and_then(Value::as_str)
        .is_some_and(|c| nodes::same_commit(c, commit));
    let list = nodes::list(v);
    server
        && !list.is_empty()
        && list.iter().all(|n| {
            n.connected
                && n.serving
                && n.up_to_date == Some(true)
                && nodes::same_commit(&n.running_commit, commit)
        })
}
