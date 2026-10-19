//! What a release source says a commit holds, and the one small form it travels in from the
//! confined fetch child to the root parent (`docs/design/kbf-installer.md` section 5,
//! "Describing a release").
//!
//! A `frontend_update` order names a commit and nothing else, so the root half asks the
//! release source what the commit holds: `ReleaseService.Describe` on a `frontend` source,
//! `<base>/releases/<commit>.json` on a `url` source. **The root half never parses the
//! source's bytes.** The fetch child ([`super::child`]), confined and unprivileged, asks,
//! decodes the protobuf or the JSON and writes the answer in the form below; a parser of a
//! network format that goes wrong goes wrong there. The parent reads that form and nothing
//! else. It is still the word of a child that read a stranger's bytes, so the parent holds
//! every field to its shape again ([`Description::select`]) before one of them names a file
//! to fetch.
//!
//! The form is lines of single-space separated words, ended by a newline:
//!
//! ```text
//! description 1 <commit>
//! component <name> <platform> <tarball> <sha256> <size> <bundle_sha256> <bundle_size>
//! ```
//!
//! one `component` line per entry, or one line `refused <text>` when the source answered that it
//! will not describe the release (an unknown commit, a platform that is not complete). A
//! description is claims, as an offer is: the stage re-derives the count, the digest, the
//! bundle's subject and the attestation of every tarball it is given.

use kbf_proto::update::ReleaseDescription;
use kbf_types::ReleasePlatform;
use kbf_update::MAX_ATTESTATION_BYTES;
use serde::Deserialize;

use super::is_digest;
use crate::CommitId;

/// The `schema` of the JSON a `url` source serves, the only one read.
pub const SCHEMA: u64 = 1;

/// The most bytes the child may write of a description, and the most of a JSON file it reads.
/// A release of three platforms is a kilobyte.
pub const MAX_BYTES: u64 = 16 * 1024;

/// The most characters of a source's refusal that are kept.
const MAX_TEXT: usize = 300;

/// One tarball of a described release, and the bundle that attests it: the fields of
/// `ReleaseComponent`, as claimed. (Also an entry of the `url` source's JSON, whose keys
/// these are; a key it does not know is ignored, so that one can be added without a new
/// schema.)
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Described {
    pub name: String,
    pub platform: String,
    /// `<name>-<commit>-<platform>.tar.gz`.
    pub tarball: String,
    pub sha256: String,
    pub size: u64,
    pub bundle_sha256: String,
    pub bundle_size: u64,
}

/// What a source says a commit holds, every platform it names.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Description {
    pub commit: String,
    pub components: Vec<Described>,
}

/// What the child reports: a description, or that the source will not give one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    Described(Description),
    /// The source answered (`NOT_FOUND`, `FAILED_PRECONDITION`, a `404`, a `409`, a file of
    /// another schema) that there is no description to be had; the words it said it in.
    Refused(String),
}

/// Why a release is not described: the source said no, or its answer cannot be used. Never a
/// source that could not be reached (that ends the child without an answer, and is a pipe
/// fault).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DescribeFault {
    #[error("the release source will not describe it: {0}")]
    Refused(String),
    #[error("the description is not usable: {0}")]
    Unusable(String),
}

impl DescribeFault {
    pub(crate) fn unusable(why: &str) -> DescribeFault {
        DescribeFault::Unusable(plain(why))
    }
}

/// `text` as one line of printable ASCII, cut to [`MAX_TEXT`] characters: a source's words
/// are shown to a person and written to a log, and carry nothing else.
pub(crate) fn plain(text: &str) -> String {
    text.chars()
        .map(|c| if (' '..='~').contains(&c) { c } else { '?' })
        .take(MAX_TEXT)
        .collect()
}

/// A field that is one word: not empty, and printable ASCII without a space.
fn word(field: &str) -> Result<&str, DescribeFault> {
    if !field.is_empty() && field.bytes().all(|b| b.is_ascii_graphic()) {
        Ok(field)
    } else {
        Err(DescribeFault::unusable(&format!(
            "a field is not one printable word: {field:?}"
        )))
    }
}

/// A count as the child writes it: decimal digits, no sign, no leading zero.
fn count(field: &str) -> Result<u64, DescribeFault> {
    field
        .parse::<u64>()
        .ok()
        .filter(|n| n.to_string() == field)
        .ok_or_else(|| DescribeFault::unusable(&format!("{field:?} is not a count")))
}

/// Just the `schema` of the `url` source's JSON, read first: a file of another schema may have
/// another shape, and is then refused for that, not for failing to fit this one.
#[derive(Deserialize)]
struct Schema {
    schema: u64,
}

impl Answer {
    /// What the `url` source's file says (`docs/api.md`): the JSON of schema [`SCHEMA`], read
    /// here in the child. A file that is not JSON of that shape, or of another schema, is a
    /// refusal that says so.
    pub fn from_json(file: &[u8]) -> Answer {
        match serde_json::from_slice::<Schema>(file) {
            Ok(Schema { schema: SCHEMA }) => {}
            Ok(Schema { schema }) => {
                return Answer::Refused(format!(
                    "the file has schema {schema}; this installer reads {SCHEMA}"
                ));
            }
            Err(e) => return Answer::Refused(format!("the file has no schema: {e}")),
        }
        match serde_json::from_slice::<Description>(file) {
            Ok(description) => Answer::Described(description),
            Err(e) => Answer::Refused(format!("the file is not a description: {e}")),
        }
    }

    /// What `ReleaseService.Describe` answered.
    pub fn from_proto(said: ReleaseDescription) -> Answer {
        Answer::Described(Description {
            commit: said.commit,
            components: said
                .components
                .into_iter()
                .map(|c| Described {
                    name: c.name,
                    platform: c.platform,
                    tarball: c.tarball,
                    sha256: c.sha256,
                    size: c.size,
                    bundle_sha256: c.bundle_sha256,
                    bundle_size: c.bundle_size,
                })
                .collect(),
        })
    }

    /// The form above. A description with a field that is not one word cannot be written, and
    /// is written as the refusal that says so.
    pub fn render(&self) -> String {
        match self {
            Answer::Refused(text) => format!("refused {}\n", plain(text)),
            Answer::Described(description) => description
                .lines()
                .unwrap_or_else(|why| format!("refused {why}\n")),
        }
    }

    /// Read what the child wrote. Exactly the form above: anything else is refused whole.
    pub fn parse(bytes: &[u8]) -> Result<Answer, DescribeFault> {
        let unusable = DescribeFault::unusable;
        let text = std::str::from_utf8(bytes).map_err(|_| unusable("it is not UTF-8"))?;
        let body = text
            .strip_suffix('\n')
            .ok_or_else(|| unusable("it does not end in a newline"))?;
        let mut lines = body.split('\n');
        let head = lines.next().unwrap_or_default();
        if let Some(said) = head.strip_prefix("refused ") {
            if lines.next().is_some() {
                return Err(unusable("a refusal is one line"));
            }
            return Ok(Answer::Refused(plain(said)));
        }
        let head: Vec<&str> = head.split(' ').collect();
        let [first, schema, commit] = head[..] else {
            return Err(unusable("the first line is not `description 1 <commit>`"));
        };
        if first != "description" || count(schema)? != SCHEMA {
            return Err(unusable("the first line is not `description 1 <commit>`"));
        }
        let mut components = Vec::new();
        for line in lines {
            let fields: Vec<&str> = line.split(' ').collect();
            let [
                first,
                name,
                platform,
                tarball,
                sha256,
                size,
                bundle_sha256,
                bundle_size,
            ] = fields[..]
            else {
                return Err(unusable("a component line has not seven fields"));
            };
            if first != "component" {
                return Err(unusable("a line after the first is not a component"));
            }
            components.push(Described {
                name: word(name)?.to_owned(),
                platform: word(platform)?.to_owned(),
                tarball: word(tarball)?.to_owned(),
                sha256: word(sha256)?.to_owned(),
                size: count(size)?,
                bundle_sha256: word(bundle_sha256)?.to_owned(),
                bundle_size: count(bundle_size)?,
            });
        }
        Ok(Answer::Described(Description {
            commit: word(commit)?.to_owned(),
            components,
        }))
    }
}

impl Described {
    /// Whether this entry is one this host may fetch: a component of `platform` that is
    /// named, sized and digested as a release is, for `commit`.
    fn check(
        &self,
        commit: &CommitId,
        platform: ReleasePlatform,
        max_tarball: u64,
    ) -> Result<(), DescribeFault> {
        let unusable = |why: String| Err(DescribeFault::unusable(&why));
        let name = &self.name;
        if !platform.components().contains(&name.as_str()) {
            return unusable(format!("{name:?} is not a component of {platform}"));
        }
        let tarball = format!("{name}-{commit}-{platform}.tar.gz");
        if self.tarball != tarball {
            return unusable(format!(
                "the file of {name} is {:?}, not {tarball}",
                self.tarball
            ));
        }
        if !is_digest(&self.sha256) || !is_digest(&self.bundle_sha256) {
            return unusable(format!("a digest of {name} is not 64 lowercase hex digits"));
        }
        if self.size == 0 || self.size > max_tarball {
            return unusable(format!(
                "{name} is {} bytes; at least 1 and at most {max_tarball}",
                self.size
            ));
        }
        if self.bundle_size == 0 || self.bundle_size > MAX_ATTESTATION_BYTES as u64 {
            return unusable(format!(
                "the bundle of {name} is {} bytes; at least 1 and at most {MAX_ATTESTATION_BYTES}",
                self.bundle_size
            ));
        }
        Ok(())
    }
}

impl Description {
    /// The `description` and `component` lines of the form above.
    fn lines(&self) -> Result<String, DescribeFault> {
        let mut out = format!("description {SCHEMA} {}\n", word(&self.commit)?);
        for c in &self.components {
            out.push_str(&format!(
                "component {} {} {} {} {} {} {}\n",
                word(&c.name)?,
                word(&c.platform)?,
                word(&c.tarball)?,
                word(&c.sha256)?,
                c.size,
                word(&c.bundle_sha256)?,
                c.bundle_size
            ));
        }
        Ok(out)
    }

    /// The components a host of `platform` fetches for `commit`: this description must be of
    /// `commit`, and hold, for `platform`, each component that platform needs once and no
    /// other, every one within the bounds a release has. A source that lists other platforms
    /// too (the `url` file lists every complete one) is not wrong: their entries are not read.
    ///
    /// `max_tarball` is the host's `max_component_bytes`.
    pub fn select(
        &self,
        commit: &CommitId,
        platform: ReleasePlatform,
        max_tarball: u64,
    ) -> Result<Vec<Described>, DescribeFault> {
        if self.commit != commit.as_str() {
            return Err(DescribeFault::unusable(&format!(
                "it describes commit {}, and the order names {commit}",
                self.commit
            )));
        }
        let ours: Vec<&Described> = self
            .components
            .iter()
            .filter(|c| c.platform == platform.as_str())
            .collect();
        for c in &ours {
            c.check(commit, platform, max_tarball)?;
        }
        let mut chosen = Vec::new();
        for need in platform.components() {
            let mut found = ours.iter().filter(|c| c.name == *need);
            match (found.next(), found.next()) {
                (Some(c), None) => chosen.push((*c).clone()),
                (None, _) => {
                    return Err(DescribeFault::unusable(&format!(
                        "it has no {need} for {platform}"
                    )));
                }
                (Some(_), Some(_)) => {
                    return Err(DescribeFault::unusable(&format!(
                        "it has {need} for {platform} twice"
                    )));
                }
            }
        }
        Ok(chosen)
    }
}

#[cfg(test)]
#[path = "describe_tests.rs"]
mod tests;
