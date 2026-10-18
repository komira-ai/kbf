//! Steps 5 and 7: wait for every node to confirm, then check the farm.

use std::collections::BTreeMap;

use serde_json::Value;

use super::api::Request;
use super::nodes::{self, Node};
use super::summary::{Exit, NodeResult};
use super::{Fail, Run, SmokeResult, fail};

impl Run<'_> {
    /// Step 5. The clock for a node starts when the previous one confirms (or at the
    /// start), so a slow storage node does not share one budget with the rest. The first
    /// node that does not confirm ends the deploy; no later node is waited for, and none
    /// is ordered: the drain system owns the roll.
    pub(super) fn wait_nodes(&mut self) -> Result<(), Fail> {
        let commit = self.opts.commit.to_string();
        let generation = self.sum.generation;
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
                        done.insert(node.id.clone(), result(node, "confirmed", now - last));
                        last = now;
                        continue;
                    }
                    // A state left from an older offer is not this deploy's.
                    let current = match (node.offered_generation, generation) {
                        (Some(offered), Some(g)) => offered >= g,
                        _ => true,
                    };
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
                        done.insert(node.id.clone(), result(node, state, now - last));
                        return Err(self.stop(list, done, &node.id, state));
                    }
                }
                if list.iter().all(|n| done.contains_key(&n.id)) {
                    self.sum.nodes = done.into_values().collect();
                    return Ok(());
                }
            }
            if now - last >= self.opts.node_timeout.as_secs() {
                let list = seen.unwrap_or_default();
                // The first node, in id order, that is still waited for.
                let stuck = list.iter().find(|n| !done.contains_key(&n.id));
                let id = stuck.map(|n| n.id.clone()).unwrap_or_default();
                if let Some(n) = stuck {
                    done.insert(id.clone(), result(n, "timed_out", now - last));
                }
                return Err(self.stop(&list, done, &id, "timed_out"));
            }
            self.env.clock.sleep(self.opts.poll);
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
        for n in list {
            done.entry(n.id.clone())
                .or_insert_with(|| result(n, "pending", 0));
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
            if ok || self.env.clock.now() - start >= self.opts.members_timeout.as_secs() {
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

/// Every node connected, serving, up to date and on the commit; the server on it too.
fn nodes_on_commit(v: &Value, commit: &str) -> bool {
    let server = v
        .pointer("/server/commit")
        .and_then(Value::as_str)
        .is_some_and(|c| nodes::same_commit(c, commit));
    server
        && nodes::list(v).iter().all(|n| {
            n.connected
                && n.serving
                && n.up_to_date == Some(true)
                && nodes::same_commit(&n.running_commit, commit)
        })
}
