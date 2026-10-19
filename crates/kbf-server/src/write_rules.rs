//! Which process may write which state-store key once another process holds the
//! generation (`docs/design/frontend-proxy.md`, section 8, "Who may write which keys").
//!
//! During a swap two frontends run over one state store. The one that took the generation
//! writes everything; the one it replaced, which still serves the calls it kept, may write
//! only what no other process writes. [`FAMILIES`] is every family of key a frontend
//! writes, found by reading the code that builds each key, with the rule it follows:
//!
//! | rule | who writes, after a takeover | what it is |
//! |---|---|---|
//! | [`Rule::Fenced`] | the generation holder only; every write is checked against the generation in its own transaction | the scheduler's rows, the member map, `update/*`, enrollment and object-store registrations, and the history index rows that every writer shares (the stream counter, the shape and status rows) |
//! | [`Rule::OwnInstance`] | the process whose id the key carries, unchecked | a history chunk row and a history header row, keyed by the writer id (`WriterId::from_instance` of the process's instance id) |
//! | [`Rule::UniqueInsert`] | any process, unchecked, as an insert (`Cond::Absent`) | a history stream row and object row, unique by allocation (the key holds a stream id from a counter block, or a run id, which no other process is given), and a list entry, whose value is empty so the same row from two writers is one row |
//!
//! The history rows follow from `kbf_history::RowKind`; [`classify`] asks it. A history
//! writer writes its rows through a handle that applies exactly the unchecked rules above
//! and sends everything else through the fence (`kbf_state::OwnKeys`), so a key of a fenced
//! family cannot reach the store unchecked, whatever a caller does.
//!
//! Not in the table, because they are not state-store keys of the frontend: the object
//! store's history objects (`history/...`, named by the writer id), the cold store, and
//! the storage node's own logs.

use std::sync::LazyLock;

use kbf_history::{IndexRoot, Ownership};

/// What a family of keys may do once the generation has moved on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rule {
    /// Written only behind the fence, by the generation holder.
    Fenced,
    /// Written unchecked, by the process whose id the key carries.
    OwnInstance,
    /// Written unchecked by any process, as an insert whose key no other process is given
    /// (or whose value is empty).
    UniqueInsert,
}

impl Rule {
    /// Whether a process that no longer holds the generation may write the family.
    #[must_use]
    pub fn survives_a_takeover(self) -> bool {
        self != Rule::Fenced
    }
}

/// How a family names its keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Keys {
    /// The one key.
    Exactly(&'static str),
    /// Every key that starts with this.
    Under(&'static str),
}

/// A family of state-store keys, as one subsystem writes them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Family {
    /// The keys.
    pub keys: Keys,
    /// The rule they follow.
    pub rule: Rule,
    /// What writes them (the module that builds the key).
    pub writer: &'static str,
    /// A key of the family, as the code builds one.
    pub example: &'static str,
}

fn fenced(keys: Keys, writer: &'static str, example: &'static str) -> Family {
    Family {
        keys,
        rule: Rule::Fenced,
        writer,
        example,
    }
}

/// Every family of keys a frontend writes outside the history index: all fenced. The
/// history index rows are classified by [`kbf_history::IndexRoot::kind`], since their root
/// is a flag.
pub static FAMILIES: LazyLock<Vec<Family>> = LazyLock::new(|| {
    vec![
        // The generation row itself: written by `Fenced::take`, which is the fence.
        fenced(
            Keys::Exactly("meta/generation"),
            "kbf-state fence",
            "meta/generation",
        ),
        // The scheduler (`farm/durable.rs`).
        fenced(Keys::Exactly("meta/epoch"), "farm/durable", "meta/epoch"),
        fenced(Keys::Exactly("meta/alive"), "farm/durable", "meta/alive"),
        fenced(
            Keys::Exactly("meta/next_op"),
            "farm/durable (counter)",
            "meta/next_op",
        ),
        fenced(
            Keys::Exactly("meta/next_seq"),
            "farm/durable (counter)",
            "meta/next_seq",
        ),
        fenced(Keys::Under("op/"), "farm/durable", "op/0000000000000001"),
        fenced(
            Keys::Under("queue/"),
            "farm/durable",
            "queue/0000000000000001",
        ),
        fenced(
            Keys::Under("loss/"),
            "farm/durable",
            "loss/0000000000000001",
        ),
        fenced(
            Keys::Under("fate/"),
            "farm/durable",
            "fate/0000000000000001",
        ),
        fenced(Keys::Under("floor/"), "farm/durable", "floor/main/0000"),
        fenced(
            Keys::Under("subject/"),
            "farm/durable",
            "subject/0000000000000001",
        ),
        fenced(
            Keys::Under("done/"),
            "farm/durable (read by the old child's follower)",
            "done/0000000000000001",
        ),
        fenced(Keys::Under("cordon/"), "farm/durable", "cordon/node-a"),
        fenced(
            Keys::Under("lease/"),
            "farm/durable",
            "lease/0000000000000001.0000000000000001",
        ),
        fenced(Keys::Under("session/"), "farm/durable", "session/node-a"),
        // The pin of the operation that runs an action: the request's instance name, not the
        // writing process's id, so it is fenced with `op/` (`farm/durable/inflight.rs`).
        fenced(
            Keys::Under("inflight/"),
            "farm/durable/inflight",
            "inflight/6d61696e/ab",
        ),
        // The member map and its rows (`members.rs`, `kbf-enroll-registry`).
        fenced(Keys::Exactly("map"), "members", "map"),
        fenced(Keys::Under("member/"), "members", "member/1"),
        fenced(Keys::Exactly("member-id"), "members (counter)", "member-id"),
        fenced(Keys::Under("boot/"), "members (counter)", "boot/1"),
        fenced(Keys::Under("instance/"), "members", "instance/node-a"),
        fenced(Keys::Under("down/"), "members", "down/1"),
        fenced(
            Keys::Exactly("meta/members_formed"),
            "members",
            "meta/members_formed",
        ),
        fenced(
            Keys::Under("deny/"),
            "members and kbf-enroll-registry",
            "deny/node/a",
        ),
        // Enrollment: tokens, nodes, serials, keys, advertised addresses, requests, renewals
        // and the CA generation, all under one prefix (`kbf-enroll-registry/src/rows.rs`).
        fenced(
            Keys::Under("enroll/"),
            "kbf-enroll-registry",
            "enroll/token/a",
        ),
        // Rolling updates (`update/rows.rs`).
        fenced(
            Keys::Exactly("update/desired"),
            "update/rows",
            "update/desired",
        ),
        fenced(
            Keys::Exactly("update/generation"),
            "update/rows (counter)",
            "update/generation",
        ),
        fenced(
            Keys::Exactly("update/policy"),
            "update/rows",
            "update/policy",
        ),
        fenced(Keys::Exactly("update/slot"), "update/rows", "update/slot"),
        fenced(
            Keys::Under("update/host/"),
            "update/rows",
            "update/host/node-a",
        ),
        fenced(
            Keys::Under("update/order/"),
            "update/rows (counter)",
            "update/order/0000000000000001",
        ),
        // Object-store registrations (`object_store/rows.rs`).
        fenced(
            Keys::Under("objstore/conn/"),
            "object_store",
            "objstore/conn/cold",
        ),
        fenced(
            Keys::Under("objstore/bind/"),
            "object_store",
            "objstore/bind/cold",
        ),
        fenced(
            Keys::Under("objstore/audit/"),
            "object_store",
            "objstore/audit/1",
        ),
        fenced(
            Keys::Exactly("objstore/gen"),
            "object_store",
            "objstore/gen",
        ),
    ]
});

/// The rule of one key, and the family it is of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Classified {
    /// The rule the key follows.
    pub rule: Rule,
    /// A name for the family: the history row kind, or the family's key pattern.
    pub family: &'static str,
}

impl Keys {
    fn matches(self, key: &str) -> bool {
        match self {
            Keys::Exactly(exact) => key == exact,
            Keys::Under(prefix) => key.starts_with(prefix),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Keys::Exactly(name) | Keys::Under(name) => name,
        }
    }
}

/// The rule `key` follows, or `None` for a key no family names. A history row is judged by
/// its kind under `index` (the root `--history-index-root` names); every other key by
/// [`FAMILIES`].
#[must_use]
pub fn classify(key: &str, index: &IndexRoot) -> Option<Classified> {
    if let Some(kind) = index.kind(key) {
        let (rule, family) = match kind.ownership() {
            Ownership::Writer(_) => (Rule::OwnInstance, "history row of one writer"),
            Ownership::Insert => (Rule::UniqueInsert, "history insert"),
            Ownership::Fenced => (Rule::Fenced, "history shared row"),
        };
        return Some(Classified { rule, family });
    }
    FAMILIES
        .iter()
        .find(|family| family.keys.matches(key))
        .map(|family| Classified {
            rule: family.rule,
            family: family.keys.name(),
        })
}

#[cfg(test)]
mod tests {
    use kbf_history::{InvKey, WriterId};

    use super::*;

    // Catches: a family whose example the family does not match (a typo in a prefix, or
    // an example that stopped being a key the code builds), and two families that claim
    // one key (the first would silently win).
    #[test]
    fn every_example_is_in_exactly_its_own_family() {
        let root = IndexRoot::default();
        for family in FAMILIES.iter() {
            assert!(family.keys.matches(family.example), "{family:?}");
            let claimed = FAMILIES
                .iter()
                .filter(|f| f.keys.matches(family.example))
                .count();
            assert_eq!(claimed, 1, "{} is claimed {claimed} times", family.example);
            let got = classify(family.example, &root).expect("classified");
            assert_eq!((got.rule, got.family), (Rule::Fenced, family.keys.name()));
        }
    }

    // Catches: a history row put in the wrong class: an own-instance row that is fenced
    // (the old child would lose its history again), a shared row that is not (two
    // children racing on one counter or status row), a unique-insert row that is not an
    // insert, and a key of no family classified as something.
    #[test]
    fn history_rows_follow_their_kind_and_unknown_keys_have_no_rule() {
        let root = IndexRoot::under("farm-a/").expect("a prefix");
        let (inv, writer) = (
            InvKey::from_record_id("b"),
            WriterId::from_instance("child"),
        );
        let own = [root.ichunk(&inv, writer, 1), root.header(&inv, writer)];
        let inserts = [
            root.stream(
                kbf_history::Day::from_number(20_261_019),
                kbf_history::StreamId(1),
                1,
            ),
            root.inv_list("main", 1, &inv),
        ];
        let shared = [
            root.streams_counter(),
            root.invid(&inv),
            root.shape("main", 1, "cfg"),
            root.status("main", 1, "cfg"),
            root.statusx("failing", "main", 1, "cfg"),
        ];
        for (keys, rule) in [
            (&own[..], Rule::OwnInstance),
            (&inserts[..], Rule::UniqueInsert),
            (&shared[..], Rule::Fenced),
        ] {
            for key in keys {
                assert_eq!(classify(key, &root).map(|c| c.rule), Some(rule), "{key}");
                assert_eq!(rule.survives_a_takeover(), rule != Rule::Fenced);
            }
        }
        assert_eq!(classify("ui/streams", &root), None, "another root's row");
        assert_eq!(classify("no-such/key", &root), None);
        assert_eq!(classify("", &root), None);
    }
}
