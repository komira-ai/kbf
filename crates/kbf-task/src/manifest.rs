//! The manifest: the record at offset 0 of a task's pack, naming everything else in it.
//!
//! The tool writes it, and the frontend reads it without trusting it: every digest it
//! names must be a record of the same pack, and the frontend rebuilds the action's result
//! from its own hashing. The wire form is JSON, version 1, with digests as
//! `<lowercase hex>/<size>` strings ([`BlobId`]).

use std::fmt;
use std::str::FromStr;

use kbf_proto::reapi;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The manifest version this build writes and reads.
pub const VERSION: u32 = 1;

/// The SHA-256 and size of one blob; its text form is `<lowercase hex>/<size>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlobId {
    /// The raw SHA-256.
    pub hash: [u8; 32],
    /// The blob's length in bytes.
    pub size: u64,
}

/// Why a string is not a [`BlobId`].
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ParseBlobIdError {
    /// There is no `/` between the hash and the size.
    #[error("digest {0:?} is not of the form <hash>/<size>")]
    MissingSeparator(String),
    /// The hash is not 64 characters of `0-9a-f`.
    #[error("digest hash {0:?} is not 64 lowercase hexadecimal characters")]
    Hash(String),
    /// The size is not a plain decimal `u64`.
    #[error("digest size {0:?} is not a non-negative decimal integer")]
    Size(String),
}

impl BlobId {
    /// The id of `bytes`.
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        let d = kbf_segments::sha256(bytes);
        Self {
            hash: d.hash,
            size: d.size_bytes,
        }
    }
}

impl fmt::Display for BlobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", hex::encode(self.hash), self.size)
    }
}

impl FromStr for BlobId {
    type Err = ParseBlobIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (hex_part, size_part) = s
            .split_once('/')
            .ok_or_else(|| ParseBlobIdError::MissingSeparator(s.to_owned()))?;
        let lower = hex_part
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        let mut hash = [0u8; 32];
        if hex_part.len() != 64 || !lower || hex::decode_to_slice(hex_part, &mut hash).is_err() {
            return Err(ParseBlobIdError::Hash(hex_part.to_owned()));
        }
        // `u64::from_str` accepts a leading `+`; one id has exactly one text form.
        if size_part.is_empty() || !size_part.bytes().all(|b| b.is_ascii_digit()) {
            return Err(ParseBlobIdError::Size(size_part.to_owned()));
        }
        let size = size_part
            .parse()
            .map_err(|_| ParseBlobIdError::Size(size_part.to_owned()))?;
        Ok(Self { hash, size })
    }
}

impl TryFrom<&reapi::Digest> for BlobId {
    type Error = ParseBlobIdError;

    fn try_from(d: &reapi::Digest) -> Result<Self, Self::Error> {
        format!("{}/{}", d.hash, d.size_bytes).parse()
    }
}

impl From<BlobId> for reapi::Digest {
    fn from(id: BlobId) -> Self {
        Self {
            hash: hex::encode(id.hash),
            // A blob is at most one segment, far below `i64::MAX`.
            size_bytes: i64::try_from(id.size).unwrap_or(i64::MAX),
        }
    }
}

impl Serialize for BlobId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for BlobId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(D::Error::custom)
    }
}

/// One output file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputFile {
    /// Path relative to the working directory.
    pub path: String,
    /// The file's content.
    pub digest: BlobId,
    /// Whether the executable bit is set.
    pub is_executable: bool,
}

/// One output directory, as the Tree that lists it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputDirectory {
    /// Path relative to the working directory.
    pub path: String,
    /// The serialized Tree.
    pub tree: BlobId,
}

/// One output symlink.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputSymlink {
    /// Path relative to the working directory.
    pub path: String,
    /// What the link points to.
    pub target: String,
}

/// Wall-clock milliseconds spent in each phase.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Timing {
    /// Fetching the action and its inputs.
    pub fetch_ms: u64,
    /// Running the command.
    pub run_ms: u64,
    /// Collecting outputs and building the pack.
    pub pack_ms: u64,
    /// Storing the pack.
    pub put_ms: u64,
}

/// What the command used.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Usage {
    /// CPU time in microseconds.
    pub cpu_usage_us: u64,
    /// Peak memory in bytes.
    pub memory_peak_bytes: u64,
}

/// The record at offset 0 of a pack.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Always [`VERSION`]; [`Manifest::parse`] refuses any other.
    pub version: u32,
    /// The lease id the frontend gave.
    pub lease: String,
    /// The action's digest. The action is an input, not a record of the pack.
    pub action: BlobId,
    /// The command's exit code.
    pub exit_code: i32,
    /// Standard output (a record of the pack).
    pub stdout: BlobId,
    /// Standard error (a record of the pack).
    pub stderr: BlobId,
    /// Output files (their contents are records of the pack).
    pub output_files: Vec<OutputFile>,
    /// Output directories (their Trees are records of the pack).
    pub output_directories: Vec<OutputDirectory>,
    /// Output symlinks.
    pub output_symlinks: Vec<OutputSymlink>,
    /// Time per phase.
    pub timing: Timing,
    /// Resource use.
    pub usage: Usage,
}

/// Why bytes are not a manifest this build reads.
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    /// Not JSON of the manifest's shape, or a digest that does not parse.
    #[error("manifest is not valid: {0}")]
    Json(#[from] serde_json::Error),
    /// A version other than [`VERSION`].
    #[error("manifest version {0} is not supported (this build reads {VERSION})")]
    Version(u32),
}

impl Manifest {
    /// The canonical JSON bytes: fields in declaration order, no whitespace.
    ///
    /// # Panics
    /// Never: every field serializes to JSON.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("a manifest is plain data and serializes to JSON")
    }

    /// Reads a manifest, refusing unknown fields, bad digests and other versions.
    ///
    /// # Errors
    /// [`ParseError`].
    pub fn parse(bytes: &[u8]) -> Result<Self, ParseError> {
        let m: Self = serde_json::from_slice(bytes)?;
        if m.version != VERSION {
            return Err(ParseError::Version(m.version));
        }
        Ok(m)
    }

    /// Every blob the manifest says the pack holds: stdout, stderr, each output file's
    /// content and each output directory's Tree. Not the action.
    pub fn blobs(&self) -> impl Iterator<Item = BlobId> + '_ {
        [self.stdout, self.stderr]
            .into_iter()
            .chain(self.output_files.iter().map(|f| f.digest))
            .chain(self.output_directories.iter().map(|d| d.tree))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn sample() -> Manifest {
        Manifest {
            version: VERSION,
            lease: "lease-1".to_owned(),
            action: BlobId::of(b"action"),
            exit_code: -3,
            stdout: BlobId::of(b"out"),
            stderr: BlobId::of(b""),
            output_files: vec![OutputFile {
                path: "bin/a".to_owned(),
                digest: BlobId::of(b"file"),
                is_executable: true,
            }],
            output_directories: vec![OutputDirectory {
                path: "d".to_owned(),
                tree: BlobId::of(b"tree"),
            }],
            output_symlinks: vec![OutputSymlink {
                path: "l".to_owned(),
                target: "bin/a".to_owned(),
            }],
            timing: Timing {
                fetch_ms: 1,
                run_ms: 2,
                pack_ms: 3,
                put_ms: 4,
            },
            usage: Usage {
                cpu_usage_us: 5,
                memory_peak_bytes: 6,
            },
        }
    }

    /// Every field survives encode then parse, and the encoding is the same each time.
    #[test]
    fn round_trip() {
        let m = sample();
        let bytes = m.encode();
        assert_eq!(Manifest::parse(&bytes).unwrap(), m);
        assert_eq!(m.encode(), bytes);
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.starts_with(r#"{"version":1,"lease":"lease-1","action":""#));
        assert!(text.contains(&format!("\"stdout\":\"{}\"", m.stdout)));
    }

    /// `blobs` names stdout, stderr, files and Trees, and not the action.
    #[test]
    fn blobs_lists_pack_records() {
        let m = sample();
        let got: Vec<BlobId> = m.blobs().collect();
        assert_eq!(
            got,
            vec![
                m.stdout,
                m.stderr,
                m.output_files[0].digest,
                m.output_directories[0].tree
            ]
        );
    }

    #[test]
    fn blob_id_text_round_trip() {
        let id = BlobId::of(b"hello");
        assert_eq!(
            id.to_string(),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824/5"
        );
        assert_eq!(id.to_string().parse::<BlobId>().unwrap(), id);
    }

    #[test]
    fn blob_id_refuses_bad_text() {
        let h = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        assert!(matches!(
            h.parse::<BlobId>(),
            Err(ParseBlobIdError::MissingSeparator(_))
        ));
        for bad in [
            "abc/5".to_owned(),
            format!("{}/5", h.to_uppercase()),
            format!("{}g/5", &h[..63]),
            format!("{h}00/5"),
            "/5".to_owned(),
        ] {
            assert!(
                matches!(bad.parse::<BlobId>(), Err(ParseBlobIdError::Hash(_))),
                "{bad}"
            );
        }
        for bad in ["", "+5", "-5", "5x", "18446744073709551616"] {
            let s = format!("{h}/{bad}");
            assert!(
                matches!(s.parse::<BlobId>(), Err(ParseBlobIdError::Size(_))),
                "{s}"
            );
        }
    }

    #[test]
    fn blob_id_reapi_conversion() {
        let id = BlobId::of(b"x");
        let d = reapi::Digest::from(id);
        assert_eq!(d.size_bytes, 1);
        assert_eq!(BlobId::try_from(&d).unwrap(), id);
        let bad = reapi::Digest {
            hash: "zz".to_owned(),
            size_bytes: 1,
        };
        assert!(BlobId::try_from(&bad).is_err());
        let negative = reapi::Digest {
            size_bytes: -1,
            ..d
        };
        assert!(BlobId::try_from(&negative).is_err());
        let huge = BlobId {
            hash: [0; 32],
            size: u64::MAX,
        };
        assert_eq!(reapi::Digest::from(huge).size_bytes, i64::MAX);
    }

    #[test]
    fn parse_refuses_bad_manifests() {
        let m = sample();
        let good = String::from_utf8(m.encode()).unwrap();
        let bad_digest = good.replace(&m.stdout.to_string(), "nothex/1");
        assert!(matches!(
            Manifest::parse(bad_digest.as_bytes()),
            Err(ParseError::Json(_))
        ));
        let version = good.replacen("\"version\":1", "\"version\":2", 1);
        assert!(matches!(
            Manifest::parse(version.as_bytes()),
            Err(ParseError::Version(2))
        ));
        let extra = good.replacen('{', "{\"extra\":1,", 1);
        assert!(Manifest::parse(extra.as_bytes()).is_err());
        assert!(Manifest::parse(b"not json").is_err());
        assert!(
            Manifest::parse(b"{}")
                .unwrap_err()
                .to_string()
                .contains("not valid")
        );
        assert!(
            Manifest::parse(version.as_bytes())
                .unwrap_err()
                .to_string()
                .contains("version 2")
        );
    }
}
