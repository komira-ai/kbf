//! The order record: the one thing the unprivileged `kbf` hands the root `kbf-installer`
//! (docs/design/kbf-installer.md section 4). A JSON file in the order spool,
//! `<epoch:020>-<order_id:020>.json`, written under a dot-prefixed temporary name and
//! renamed into place, that says what to do and, for an activation, where the release can
//! be fetched and what it is expected to declare.
//!
//! Everything in it is a claim by the service user: the installer re-derives whatever
//! matters (the digest, the attestation, the binary's own report) before it changes
//! anything. This module owns the format for both ends, so the writer cannot make what the
//! reader refuses and the reader's limits are the writer's. The rules on `generation`
//! belong to the installer; here it is only carried.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::offer::{Refusal, check_component};
use crate::types::{Offer, Order, Verb, is_hex};

/// The most bytes a record may be; the installer refuses a larger file unread.
pub const MAX_RECORD_BYTES: u64 = 1 << 20;

/// The most records the spool holds; the installer deletes the excess unread.
pub const MAX_RECORDS: usize = 64;

/// How long a record is good for, in seconds, counted by the installer from when it first
/// saw the record: the longest confirm window. An older one is expired and removed unread,
/// so nothing replays a stale order after a long stop.
pub const MAX_AGE_SECS: u64 = 1800;

/// The suffix of a record's file.
pub const SUFFIX: &str = ".json";

/// Where one component of the release can be fetched, as the offer named it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Locator {
    pub name: String,
    pub sha256: String,
    pub size: u64,
    /// The build attestation (a Sigstore bundle line), as text.
    pub attestation: String,
}

/// What the release is expected to declare about itself (`kbf --version --json`), as the
/// frontend's preflight recorded it. The installer requires the fetched binary to say the
/// same, before it switches to it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Facts {
    pub platform: String,
    pub protocol: u32,
    pub min_protocol: u32,
    pub state_schema: u32,
    pub profile: String,
}

/// One order, as the spool holds it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrderRecord {
    pub order_id: u64,
    pub epoch: u64,
    /// The generation of the desired release; see `UpdateOrder.generation`.
    pub generation: u64,
    pub verb: Verb,
    pub commit: String,
    pub window_s: u32,
    /// For an activation: the components the platform needs. Empty for the other verbs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub components: Vec<Locator>,
    /// For an activation: what the fetched binary must declare. Absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected: Option<Facts>,
}

/// Why a record cannot be written or is not acceptable.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RecordError {
    #[error("commit {0:?} is not 40 lowercase hex digits")]
    Commit(String),
    #[error("a node never writes a frontend_update record")]
    NotForNodes,
    #[error("an activation names no release component")]
    NoComponents,
    #[error("a {0} order carries components or expected facts")]
    Unexpected(&'static str),
    #[error("an activation carries no expected facts of the release")]
    NoFacts,
    #[error("the expected facts are incomplete: {0}")]
    Facts(String),
    #[error("component {0:?} has an attestation that is not text")]
    AttestationNotText(String),
    #[error("{0}")]
    Component(#[from] Refusal),
    #[error("the record is {0} bytes; at most {MAX_RECORD_BYTES}")]
    TooLarge(usize),
    #[error("not a record: {0}")]
    Parse(String),
}

/// The file name of the record for `epoch` and `order_id`: sorts by (epoch, id).
#[must_use]
pub fn file_name(epoch: u64, order_id: u64) -> String {
    format!("{epoch:020}-{order_id:020}{SUFFIX}")
}

/// The (epoch, id) a file name stands for: twenty digits, a dash, twenty digits, `.json`.
/// A name that starts with a dot (a record being written) is none.
#[must_use]
pub fn parse_file_name(name: &str) -> Option<(u64, u64)> {
    let stem = name.strip_suffix(SUFFIX)?;
    let (epoch, id) = stem.split_once('-')?;
    let digits = |t: &str| t.len() == 20 && t.bytes().all(|b| b.is_ascii_digit());
    if !digits(epoch) || !digits(id) {
        return None;
    }
    Some((epoch.parse().ok()?, id.parse().ok()?))
}

impl OrderRecord {
    /// The record for `order` with `verb`. An activation takes its locators and expected
    /// facts from `offer`, which must be the offer this node holds for the order's commit.
    ///
    /// # Errors
    /// An activation without an offer, with an offer that carries no expected facts or an
    /// attestation that is not text, or a record that would be too large; any record
    /// that fails [`check`](Self::check). A frontend update, which a node never writes.
    pub fn for_order(
        order: &Order,
        verb: Verb,
        offer: Option<&Offer>,
    ) -> Result<Self, RecordError> {
        if verb == Verb::FrontendUpdate {
            return Err(RecordError::NotForNodes);
        }
        let (components, expected) = match (verb, offer) {
            (Verb::Activate, Some(offer)) => {
                let components = offer
                    .components
                    .iter()
                    .map(|c| {
                        Ok(Locator {
                            name: c.name.clone(),
                            sha256: c.sha256.clone(),
                            size: c.size,
                            attestation: String::from_utf8(c.attestation.clone())
                                .map_err(|_| RecordError::AttestationNotText(c.name.clone()))?,
                        })
                    })
                    .collect::<Result<Vec<_>, RecordError>>()?;
                let e = &offer.expected;
                let facts = Facts {
                    platform: offer.platform.clone(),
                    protocol: e.protocol,
                    min_protocol: e.min_protocol,
                    state_schema: e.state_schema,
                    profile: e.profile.clone(),
                };
                (components, Some(facts))
            }
            _ => (Vec::new(), None),
        };
        let record = Self {
            order_id: order.order_id,
            epoch: order.epoch,
            generation: order.generation,
            verb,
            commit: order.commit.clone(),
            window_s: order.window_s,
            components,
            expected,
        };
        record.check()?;
        record.to_bytes()?;
        Ok(record)
    }

    /// The file this record is stored in.
    #[must_use]
    pub fn file_name(&self) -> String {
        file_name(self.epoch, self.order_id)
    }

    /// The bytes to store.
    ///
    /// # Errors
    /// More than [`MAX_RECORD_BYTES`].
    pub fn to_bytes(&self) -> Result<Vec<u8>, RecordError> {
        let bytes = serde_json::to_vec(self).expect("plain fields serialize");
        if bytes.len() as u64 > MAX_RECORD_BYTES {
            return Err(RecordError::TooLarge(bytes.len()));
        }
        Ok(bytes)
    }

    /// Reads and checks a record. An unknown or repeated key, a missing one and anything
    /// [`check`](Self::check) refuses are errors.
    ///
    /// # Errors
    /// The bytes are not a record the installer may act on.
    pub fn parse(bytes: &[u8]) -> Result<Self, RecordError> {
        if bytes.len() as u64 > MAX_RECORD_BYTES {
            return Err(RecordError::TooLarge(bytes.len()));
        }
        let record: Self =
            serde_json::from_slice(bytes).map_err(|e| RecordError::Parse(e.to_string()))?;
        record.check()?;
        Ok(record)
    }

    /// The shape rules both ends hold: a full commit; an activation names its components
    /// (each within the offer's limits) and the expected facts, every one there (a release
    /// the frontend never preflighted is not activated); every other verb carries neither.
    /// A `frontend_update` is a locator like a `confirm`: the root half fetches the commit
    /// from its own source and re-derives every fact, so the record names no file, no
    /// digest and no claim.
    ///
    /// # Errors
    /// The first rule broken.
    pub fn check(&self) -> Result<(), RecordError> {
        if !is_hex(&self.commit, 40) {
            return Err(RecordError::Commit(self.commit.clone()));
        }
        match self.verb {
            Verb::Activate => {}
            Verb::Confirm | Verb::Revert | Verb::FrontendUpdate => {
                if !self.components.is_empty() || self.expected.is_some() {
                    return Err(RecordError::Unexpected(self.verb.name()));
                }
                return Ok(());
            }
        }
        if self.components.is_empty() {
            return Err(RecordError::NoComponents);
        }
        let mut seen = BTreeSet::new();
        for c in &self.components {
            check_component(&c.name, &c.sha256, c.size, c.attestation.len(), &mut seen)?;
        }
        // The expected facts are the fail-closed claim of an activation.
        let Some(f) = &self.expected else {
            return Err(RecordError::NoFacts);
        };
        if f.platform.is_empty() || f.profile.is_empty() {
            return Err(RecordError::Facts("no platform or profile".to_owned()));
        }
        if f.protocol == 0 || f.state_schema == 0 {
            return Err(RecordError::Facts("no protocol or state schema".to_owned()));
        }
        if f.min_protocol > f.protocol {
            return Err(RecordError::Facts(format!(
                "min_protocol {} is above protocol {}",
                f.min_protocol, f.protocol
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::{COMMIT, PLATFORM, offer};
    use crate::types::Expected;

    fn good_offer() -> Offer {
        Offer {
            expected: Expected {
                protocol: 7,
                min_protocol: 6,
                state_schema: 3,
                profile: "release".to_owned(),
            },
            ..offer()
        }
    }

    fn order(commit: &str) -> Order {
        Order {
            order_id: 5,
            epoch: 2,
            generation: 9,
            verb: Some(Verb::Activate),
            commit: commit.to_owned(),
            window_s: 600,
        }
    }

    /// Catches a record that does not survive its own bytes (the writer and reader
    /// disagree), loses a field, or does not carry the offer's locators and facts: every
    /// field has a different value so a swap shows.
    #[test]
    fn an_activation_record_round_trips_with_the_offers_locators_and_facts() {
        let record = OrderRecord::for_order(&order(COMMIT), Verb::Activate, Some(&good_offer()))
            .expect("record");
        assert_eq!(record.generation, 9);
        assert_eq!(record.components.len(), 1);
        assert_eq!(record.components[0].size, 1000);
        let facts = record.expected.clone().expect("facts");
        assert_eq!(
            (
                facts.platform.as_str(),
                facts.protocol,
                facts.min_protocol,
                facts.state_schema
            ),
            (PLATFORM, 7, 6, 3)
        );
        let again = OrderRecord::parse(&record.to_bytes().expect("bytes")).expect("parses");
        assert_eq!(again, record);
        assert_eq!(record.file_name(), format!("{:020}-{:020}.json", 2, 5));
    }

    /// Catches the record limits: an offer with no expected facts is not activated (fail
    /// closed), an attestation that is not text, a confirm that carries components, a
    /// commit that is not a commit, and a record over 1 MiB each have their own refusal.
    #[test]
    fn a_record_the_installer_would_refuse_is_not_written() {
        let o = order(COMMIT);
        assert!(matches!(
            OrderRecord::for_order(&o, Verb::Activate, Some(&offer())),
            Err(RecordError::Facts(_))
        ));
        assert_eq!(
            OrderRecord::for_order(&o, Verb::Activate, None).unwrap_err(),
            RecordError::NoComponents
        );
        let mut binary = good_offer();
        binary.components[0].attestation = vec![0xff, 0xfe];
        assert_eq!(
            OrderRecord::for_order(&o, Verb::Activate, Some(&binary)).unwrap_err(),
            RecordError::AttestationNotText("kbf".to_owned())
        );
        let mut confirm = OrderRecord::for_order(&o, Verb::Confirm, None).expect("confirm");
        assert_eq!(
            (confirm.components.is_empty(), confirm.expected.is_none()),
            (true, true)
        );
        confirm.components = vec![Locator {
            name: "kbf".to_owned(),
            sha256: "ab".repeat(32),
            size: 1,
            attestation: "x".to_owned(),
        }];
        assert_eq!(confirm.check(), Err(RecordError::Unexpected("confirm")));
        assert_eq!(
            OrderRecord::for_order(&order("not-a-commit"), Verb::Revert, None).unwrap_err(),
            RecordError::Commit("not-a-commit".to_owned())
        );
        let mut record =
            OrderRecord::for_order(&o, Verb::Activate, Some(&good_offer())).expect("ok");
        record.components[0].attestation = "a".repeat(2 << 20);
        assert!(matches!(record.to_bytes(), Err(RecordError::TooLarge(_))));
    }

    /// Catches a shape rule of `check` that is missing or has the wrong bound, on the reader's
    /// side as on the writer's: each break of a good activation record is refused with its own
    /// reason (no components, no expected facts at all, no platform, no profile, protocol 0,
    /// state schema 0, a minimum protocol above the protocol), the bound is inclusive (equal
    /// is accepted), a confirm carrying only expected facts is refused as a confirm with
    /// extras, and the word of every verb is the one the file carries.
    ///
    /// Mutants planted: `min_protocol >= protocol` (equal is refused); the components check
    /// removed (an activation with none is accepted); the facts check on `||` replaced by `&&`
    /// (an empty platform with a profile is accepted); the facts-required arm made to pass an
    /// activation with none (the "no facts" break is accepted).
    #[test]
    fn a_record_that_breaks_a_shape_rule_is_refused() {
        let good = OrderRecord::for_order(&order(COMMIT), Verb::Activate, Some(&good_offer()))
            .expect("record");
        assert_eq!(good.check(), Ok(()));
        type Break = (&'static str, fn(&mut OrderRecord), &'static str);
        let breaks: [Break; 7] = [
            (
                "components",
                |r| r.components.clear(),
                "names no release component",
            ),
            (
                "no facts",
                |r| r.expected = None,
                "carries no expected facts",
            ),
            (
                "platform",
                |r| r.expected.as_mut().expect("facts").platform.clear(),
                "no platform or profile",
            ),
            (
                "profile",
                |r| r.expected.as_mut().expect("facts").profile.clear(),
                "no platform or profile",
            ),
            (
                "protocol",
                |r| r.expected.as_mut().expect("facts").protocol = 0,
                "no protocol or state schema",
            ),
            (
                "schema",
                |r| r.expected.as_mut().expect("facts").state_schema = 0,
                "no protocol or state schema",
            ),
            (
                "minimum",
                |r| r.expected.as_mut().expect("facts").min_protocol = 8,
                "min_protocol 8 is above protocol 7",
            ),
        ];
        for (what, change, want) in breaks {
            let mut r = good.clone();
            change(&mut r);
            let err = r.check().unwrap_err().to_string();
            assert!(err.contains(want), "{what}: {err}");
        }
        let mut equal = good.clone();
        equal.expected.as_mut().expect("facts").min_protocol = 7;
        assert_eq!(
            equal.check(),
            Ok(()),
            "a minimum equal to the protocol is allowed"
        );
        let mut extras =
            OrderRecord::for_order(&order(COMMIT), Verb::Confirm, None).expect("confirm");
        extras.expected = good.expected.clone();
        assert_eq!(extras.check(), Err(RecordError::Unexpected("confirm")));
        let words: Vec<_> = [
            Verb::Activate,
            Verb::Confirm,
            Verb::Revert,
            Verb::FrontendUpdate,
        ]
        .into_iter()
        .map(Verb::name)
        .collect();
        assert_eq!(words, ["activate", "confirm", "revert", "frontend_update"]);
    }

    /// The golden bytes of a `frontend_update` record: a locator and nothing else, the six
    /// keys of every record and no `components` or `expected` (the installer fetches the
    /// commit from its own source and re-derives every fact, so the record cannot name a
    /// path, a URL or a digest). It round-trips byte for byte; the same bytes as an
    /// activation have no files to fetch; adding components, facts or both is refused
    /// whole, each with the reason that names the verb.
    ///
    /// Mutants planted: the old rule restored (a `frontend_update` must name components, so
    /// the locator is refused); the check widened so a `frontend_update` may carry components
    /// (the `components` case is accepted); the same for facts (the `facts` case).
    #[test]
    fn a_frontend_update_record_is_a_locator_and_nothing_else() {
        let golden = format!(
            "{{\"order_id\":5,\"epoch\":2,\"generation\":9,\"verb\":\"frontend_update\",\"commit\":\"{COMMIT}\",\"window_s\":600}}"
        );
        let record = OrderRecord::parse(golden.as_bytes()).expect("the locator parses");
        assert_eq!(record.verb, Verb::FrontendUpdate);
        assert!(record.components.is_empty());
        assert!(record.expected.is_none());
        assert_eq!(record.to_bytes().expect("bytes"), golden.as_bytes());
        let as_activate = golden.replace("frontend_update", "activate");
        assert_eq!(
            OrderRecord::parse(as_activate.as_bytes()),
            Err(RecordError::NoComponents)
        );
        let sha = "ab".repeat(32);
        let components = format!(
            "\"components\":[{{\"name\":\"kbf\",\"sha256\":\"{sha}\",\"size\":1000,\"attestation\":\"{{}}\"}}]"
        );
        let facts = "\"expected\":{\"platform\":\"linux-x86_64\",\"protocol\":7,\"min_protocol\":6,\"state_schema\":3,\"profile\":\"release\"}";
        for (what, extra) in [
            ("facts", facts.to_owned()),
            ("components", components.clone()),
            ("both", format!("{components},{facts}")),
        ] {
            let bytes = golden.replace("600}", &format!("600,{extra}}}"));
            assert_eq!(
                OrderRecord::parse(bytes.as_bytes()),
                Err(RecordError::Unexpected("frontend_update")),
                "{what}"
            );
        }
    }

    /// Catches a node writing a frontend update: `for_order` refuses that verb whatever
    /// offer it is given, no wire verb value maps to it, and the word is the contract's
    /// `frontend_update` in a record (which the reader, not this crate's writer, takes).
    ///
    /// Mutants planted: the guard in `for_order` removed; the wire's catch-all arm made to
    /// map to it.
    #[test]
    fn a_node_never_writes_a_frontend_update() {
        let o = order(COMMIT);
        assert_eq!(
            OrderRecord::for_order(&o, Verb::FrontendUpdate, Some(&good_offer())).unwrap_err(),
            RecordError::NotForNodes
        );
        assert_eq!(
            OrderRecord::for_order(&o, Verb::FrontendUpdate, None).unwrap_err(),
            RecordError::NotForNodes
        );
        for value in -2..=100 {
            let mapped = Order::from(kbf_proto::update::UpdateOrder {
                verb: value,
                commit: COMMIT.to_owned(),
                ..Default::default()
            });
            assert_ne!(mapped.verb, Some(Verb::FrontendUpdate), "wire verb {value}");
        }
        // The word, as the reader sees it from the frontend's own writer: a confirm's
        // record (a locator) with the verb changed.
        let mut by_hand = OrderRecord::for_order(&o, Verb::Confirm, None).expect("ok");
        by_hand.verb = Verb::FrontendUpdate;
        let text = String::from_utf8(by_hand.to_bytes().expect("bytes")).expect("text");
        assert!(text.contains("\"verb\":\"frontend_update\""), "{text}");
        assert_eq!(
            OrderRecord::parse(text.as_bytes()).expect("parses"),
            by_hand
        );
    }

    /// Catches a reader that accepts what the writer never makes: an extra key (a path or a
    /// command smuggled in), a repeated key (the second silently winning), a body over the
    /// cap; and file names that are not exactly twenty digits, a dash, twenty digits and
    /// `.json` (a dot-prefixed temporary name among them).
    #[test]
    fn only_the_writers_shape_is_read() {
        let record = OrderRecord::for_order(&order(COMMIT), Verb::Confirm, None).expect("record");
        let text = String::from_utf8(record.to_bytes().expect("bytes")).expect("text");
        let extra = text.replace('}', ",\"path\":\"/etc\"}");
        assert!(matches!(
            OrderRecord::parse(extra.as_bytes()),
            Err(RecordError::Parse(_))
        ));
        let twice = text.replace('{', "{\"order_id\":6,");
        let duplicate = OrderRecord::parse(twice.as_bytes()).unwrap_err();
        assert!(duplicate.to_string().contains("duplicate"), "{duplicate}");
        assert!(matches!(
            OrderRecord::parse(&vec![b' '; (MAX_RECORD_BYTES + 1) as usize]),
            Err(RecordError::TooLarge(_))
        ));
        assert_eq!(parse_file_name(&file_name(2, 5)), Some((2, 5)));
        for bad in [
            "5.json",
            ".partial-0123.json",
            ".00000000000000000002-00000000000000000005.json",
            "00000000000000000002-5.json",
            "00000000000000000002_00000000000000000005.json",
            "0000000000000000000a-00000000000000000005.json",
            "00000000000000000002-00000000000000000005.txt",
            "00000000000000000002-00000000000000000005",
            "99999999999999999999-00000000000000000005.json",
        ] {
            assert_eq!(parse_file_name(bad), None, "{bad}");
        }
    }
}
