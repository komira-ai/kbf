//! The result document: one shape for every stage.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::gate::{GateResult, Verdict};

/// The document's format number. A reader refuses any other.
pub const SCHEMA: u32 = 1;

/// Why a document is refused.
#[derive(Debug, thiserror::Error)]
pub enum DocError {
    /// The bytes are not a result document.
    #[error("not a result document: {0}")]
    Json(#[from] serde_json::Error),
    /// The document holds a number JSON cannot write.
    #[error("gate `{0}` has a limit that is not a number")]
    NotANumber(String),
    /// The document is for another format number.
    #[error("result document schema {found}, this reader knows {SCHEMA}")]
    Schema {
        /// The number the document carries.
        found: u32,
    },
    /// The document says green but its gates do not.
    #[error("result document claims green but {0}")]
    GreenButNot(&'static str),
    /// A waiver without who or why.
    #[error("waiver for `{0}` names no operator or no reason")]
    Waiver(String),
    /// An offer document a reader refuses.
    #[error("offer document: {0}")]
    Offer(String),
    /// A hash that is not lower-case hex of the right length.
    #[error("`{0}` is not a lower-case hex string of {1} characters")]
    Hex(String, &'static str),
}

fn hex_of(s: &str, lens: &[usize], what: &'static str) -> Result<String, DocError> {
    let ok = lens.contains(&s.len()) && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    if ok {
        Ok(s.to_owned())
    } else {
        Err(DocError::Hex(s.to_owned(), what))
    }
}

/// A commit id: 40 (SHA-1) or 64 (SHA-256) lower-case hex characters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CommitId(String);

impl CommitId {
    /// The commit id in `s`, or why it is not one.
    pub fn parse(s: &str) -> Result<Self, DocError> {
        hex_of(s, &[40, 64], "40 or 64").map(Self)
    }

    /// The hex text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for CommitId {
    type Error = DocError;
    fn try_from(s: String) -> Result<Self, DocError> {
        Self::parse(&s)
    }
}

impl From<CommitId> for String {
    fn from(c: CommitId) -> String {
        c.0
    }
}

/// The hash of a release set: the SHA-256 of the `SHA256SUMS` file's bytes, as 64
/// lower-case hex characters. [`SetHash::of_sums`] is its only definition; `cargo xtask`
/// and the bake evaluator both call it, so the two cannot differ.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SetHash(String);

impl SetHash {
    /// The hash of the set whose `SHA256SUMS` file holds exactly `sums`.
    pub fn of_sums(sums: &[u8]) -> Self {
        let digest = Sha256::digest(sums);
        Self(digest.iter().map(|b| format!("{b:02x}")).collect())
    }

    /// The set hash in `s`, or why it is not one.
    pub fn parse(s: &str) -> Result<Self, DocError> {
        hex_of(s, &[64], "64").map(Self)
    }

    /// The hex text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for SetHash {
    type Error = DocError;
    fn try_from(s: String) -> Result<Self, DocError> {
        Self::parse(&s)
    }
}

impl From<SetHash> for String {
    fn from(h: SetHash) -> String {
        h.0
    }
}

/// The stage a document is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stage {
    /// The hosted-runner farm run on every merge.
    Beta,
    /// The operator's own farm: the deploy and the 24 hour bake.
    Gamma,
    /// The publish.
    Prod,
}

/// Where a run is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Written first; also what a killed process leaves behind.
    Running,
    /// Finished, and every gate passed (or only warned).
    Green,
    /// Finished with a failed gate, or ended without a result.
    Red,
    /// Finished without a decision (a bake with too little load, an evaluator error): not a pass.
    Extended,
}

/// A span of unix seconds; `end` is absent while it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Window {
    /// When it began.
    pub start: u64,
    /// When it ended.
    pub end: Option<u64>,
}

/// One minimum-load number: what the window measured against what it needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Floor {
    /// What the window measured.
    pub measured: u64,
    /// What it needs.
    pub required: u64,
}

impl Floor {
    /// Whether the floor is met: equal passes, one below fails.
    pub fn met(&self) -> bool {
        self.measured >= self.required
    }
}

/// The bake's minimum-load numbers (section 7): a bake with no usage is not a pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MinLoad {
    /// Executed actions.
    pub actions: Floor,
    /// Builds.
    pub builds: Floor,
    /// Distinct hours with load.
    pub hours: Floor,
}

impl MinLoad {
    /// Whether all three floors are met.
    pub fn met(&self) -> bool {
        self.actions.met() && self.builds.met() && self.hours.met()
    }
}

/// An operator's waiver of one condition; the verdict records it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Waiver {
    /// The gate or condition waived.
    pub what: String,
    /// Who waived it.
    pub by: String,
    /// Why.
    pub reason: String,
}

impl Waiver {
    /// A waiver, refused when it names no operator or no reason.
    pub fn new(what: &str, by: &str, reason: &str) -> Result<Self, DocError> {
        if by.trim().is_empty() || reason.trim().is_empty() {
            return Err(DocError::Waiver(what.to_owned()));
        }
        Ok(Self {
            what: what.to_owned(),
            by: by.to_owned(),
            reason: reason.to_owned(),
        })
    }
}

/// What a run knows when it starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    /// The stage.
    pub stage: Stage,
    /// The commit under test.
    pub commit: CommitId,
    /// The release set under test.
    pub set_hash: SetHash,
    /// The CI run, or the evaluator's own id.
    pub run_id: String,
    /// The attempt of this run id (1 for the first).
    pub attempt: u32,
}

/// The result document of one stage run (`gamma-verdict.json` for the gamma bake).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResultDoc {
    /// [`SCHEMA`].
    pub schema: u32,
    /// The stage.
    pub stage: Stage,
    /// The commit under test.
    pub commit: CommitId,
    /// The release set under test.
    pub set_hash: SetHash,
    /// The CI run, or the evaluator's own id.
    pub run_id: String,
    /// The attempt of this run id.
    pub attempt: u32,
    /// Where the run is.
    pub status: Status,
    /// When the run began (unix seconds).
    pub started: u64,
    /// When it finished; absent while `Running`.
    pub finished: Option<u64>,
    /// The generation the deploy set as desired.
    pub desired_generation: Option<u64>,
    /// The deploy's span.
    pub deploy: Option<Window>,
    /// The bake's span.
    pub bake: Option<Window>,
    /// The bake's minimum-load numbers.
    pub load: Option<MinLoad>,
    /// The waivers an operator recorded.
    pub waivers: Vec<Waiver>,
    /// Each gate, in the order it was decided.
    pub gates: Vec<GateResult>,
    /// Why a run ended without deciding, or was dropped.
    pub note: Option<String>,
    /// The release tag a gamma verdict is for; `release verdict` refuses another.
    pub tag: Option<String>,
    /// What makes the release not reversible (section 8.3), one line each; the notes carry it.
    pub not_reversible: Vec<String>,
    /// The scenarios only gamma found (section 10), one line each; the notes count them.
    pub escapes: Vec<String>,
    /// The earlier releases this one withdraws (section 8.2); the notes name them.
    pub withdrawn: Vec<String>,
}

impl ResultDoc {
    /// A `Running` document for `header`, begun at `now`.
    pub fn begin(header: Header, now: u64) -> Self {
        Self {
            schema: SCHEMA,
            stage: header.stage,
            commit: header.commit,
            set_hash: header.set_hash,
            run_id: header.run_id,
            attempt: header.attempt,
            status: Status::Running,
            started: now,
            finished: None,
            desired_generation: None,
            deploy: None,
            bake: None,
            load: None,
            waivers: Vec::new(),
            gates: Vec::new(),
            note: None,
            tag: None,
            not_reversible: Vec::new(),
            escapes: Vec::new(),
            withdrawn: Vec::new(),
        }
    }

    /// The status the gates allow: green only when there is at least one gate and none is red.
    /// A document with no gate decided nothing.
    pub fn status_from_gates(&self) -> Status {
        let any_red = self.gates.iter().any(|g| g.verdict == Verdict::Red);
        if self.gates.is_empty() || any_red {
            Status::Red
        } else {
            Status::Green
        }
    }

    /// Whether this document passes: finished green. `Running` (a killed process), `Red` and
    /// `Extended` do not.
    pub fn passed(&self) -> bool {
        self.status == Status::Green
    }

    /// The document as it is written: pretty JSON and a newline.
    pub fn to_bytes(&self) -> Result<Vec<u8>, DocError> {
        if let Some(g) = self.gates.iter().find(|g| !g.threshold.is_finite()) {
            return Err(DocError::NotANumber(g.name.clone()));
        }
        let mut bytes = serde_json::to_vec_pretty(self)?;
        bytes.push(b'\n');
        Ok(bytes)
    }

    /// Read a document, refusing a wrong schema, an unknown field, a waiver without a reason
    /// and a green status the gates do not support.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, DocError> {
        let doc: Self = serde_json::from_slice(bytes)?;
        if doc.schema != SCHEMA {
            return Err(DocError::Schema { found: doc.schema });
        }
        for w in &doc.waivers {
            Waiver::new(&w.what, &w.by, &w.reason)?;
        }
        if doc.status == Status::Green {
            if doc.gates.is_empty() {
                return Err(DocError::GreenButNot("it has no gate"));
            }
            if doc.gates.iter().any(|g| g.verdict == Verdict::Red) {
                return Err(DocError::GreenButNot("a gate is red"));
            }
            if doc.finished.is_none() {
                return Err(DocError::GreenButNot("it has no finish time"));
            }
        }
        Ok(doc)
    }
}
