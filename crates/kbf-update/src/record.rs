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
        let bytes = serde_json::to_vec(self).map_err(|e| RecordError::Parse(e.to_string()))?;
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
    /// the frontend never preflighted is not activated); a frontend update names its
    /// components and may leave the facts out; the other verbs carry neither.
    ///
    /// # Errors
    /// The first rule broken.
    pub fn check(&self) -> Result<(), RecordError> {
        if !is_hex(&self.commit, 40) {
            return Err(RecordError::Commit(self.commit.clone()));
        }
        if !matches!(self.verb, Verb::Activate | Verb::FrontendUpdate) {
            if !self.components.is_empty() || self.expected.is_some() {
                return Err(RecordError::Unexpected(self.verb.name()));
            }
            return Ok(());
        }
        if self.components.is_empty() {
            return Err(RecordError::NoComponents);
        }
        let mut seen = BTreeSet::new();
        for c in &self.components {
            check_component(&c.name, &c.sha256, c.size, c.attestation.len(), &mut seen)?;
        }
        // The expected facts are the fail-closed claim of an activation. A frontend update
        // is built from the frontend's own release store, which holds none: optional there,
        // and complete when given.
        let Some(f) = &self.expected else {
            return if self.verb == Verb::FrontendUpdate {
                Ok(())
            } else {
                Err(RecordError::NoFacts)
            };
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
        assert!(confirm.components.is_empty() && confirm.expected.is_none());
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

    /// The golden fixture of a `frontend_update` record: the exact bytes the frontend's writer
    /// and the installer's reader agree on. Facts are optional for it and complete when
    /// present; an activation without them is refused (fail closed).
    ///
    /// Mutant planted: the exemption widened to every verb (the activation is then accepted).
    #[test]
    fn a_frontend_update_record_is_the_golden_bytes_and_its_facts_are_optional() {
        let sha = "ab".repeat(32);
        let golden = format!(
            "{{\"order_id\":5,\"epoch\":2,\"generation\":9,\"verb\":\"frontend_update\",\"commit\":\"{COMMIT}\",\"window_s\":600,\"components\":[{{\"name\":\"kbf\",\"sha256\":\"{sha}\",\"size\":1000,\"attestation\":\"{{}}\"}}]}}"
        );
        let record = OrderRecord::parse(golden.as_bytes()).expect("parses without facts");
        assert_eq!(record.verb, Verb::FrontendUpdate);
        assert!(record.expected.is_none());
        assert_eq!(record.to_bytes().expect("bytes"), golden.as_bytes());
        // The same record as an activation has no claims to stand on.
        let as_activate = golden.replace("frontend_update", "activate");
        assert_eq!(
            OrderRecord::parse(as_activate.as_bytes()),
            Err(RecordError::NoFacts)
        );
        // Given, the facts are held to the same rules.
        let with_bad_facts = golden.replace(
            "}]}",
            "}],\"expected\":{\"platform\":\"linux-x86_64\",\"protocol\":0,\"min_protocol\":0,\"state_schema\":3,\"profile\":\"release\"}}",
        );
        assert!(matches!(
            OrderRecord::parse(with_bad_facts.as_bytes()),
            Err(RecordError::Facts(_))
        ));
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
        // The word, as the reader sees it from the frontend's own writer.
        let mut by_hand =
            OrderRecord::for_order(&o, Verb::Activate, Some(&good_offer())).expect("ok");
        by_hand.verb = Verb::FrontendUpdate;
        let text = String::from_utf8(by_hand.to_bytes().expect("bytes")).expect("text");
        assert!(text.contains("\"verb\":\"frontend_update\""), "{text}");
        assert_eq!(
            OrderRecord::parse(text.as_bytes()).expect("parses"),
            by_hand
        );
        // A frontend update carries claims like an activation; without them it is refused.
        by_hand.components.clear();
        assert_eq!(by_hand.check(), Err(RecordError::NoComponents));
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
        assert!(
            matches!(OrderRecord::parse(twice.as_bytes()), Err(RecordError::Parse(e)) if e.contains("duplicate"))
        );
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
