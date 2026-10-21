//! The version facts of one platform of a release: what its `kbf --version --json` says.
//!
//! The document is stored per commit and platform as the exact bytes of that report, named
//! `kbf-<commit>-<platform>.version.json` ([`super::key::facts_file`]), and read with the one
//! parser of it, [`kbf_types::Version`], which the installer reads the same report with. An
//! offer carries the facts ([`Facts`]) so a node can refuse a release it cannot run before it
//! fetches it. They are a hint: the installer still runs the staged binary and requires its
//! own report to equal them.

use kbf_types::{ReleasePlatform, Version};

use super::key::Commit;

/// The facts an offer carries, from the document of a platform.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Facts {
    pub protocol: u32,
    pub min_protocol: u32,
    pub state_schema: u32,
    pub profile: String,
}

impl Facts {
    /// The facts `doc` states for `platform` of `commit`.
    ///
    /// # Errors
    /// Why the document is not one: it does not parse as a `--version --json` report, it names
    /// another commit than `commit` (the first 12 digits) or another platform, it is not a
    /// release build, or a number is one the node refuses (a `protocol` or `state_schema` of 0
    /// or past 32 bits, a `min_protocol` past 32 bits or above `protocol`).
    pub fn of_document(
        commit: &Commit,
        platform: ReleasePlatform,
        doc: &[u8],
    ) -> Result<Self, String> {
        let version = Version::parse(doc).map_err(|e| e.to_string())?;
        version.check(commit.as_str()).map_err(|e| e.to_string())?;
        if let Some(said) = version.platform.as_deref()
            && said != platform.as_str()
        {
            return Err(format!(
                "it says platform {said}, and the upload is for {platform}"
            ));
        }
        let number = |name: &str, value: u64| {
            u32::try_from(value).map_err(|_| format!("{name} {value} does not fit 32 bits"))
        };
        let (protocol, min_protocol, state_schema) = (
            number("protocol", version.protocol)?,
            number("min_protocol", version.min_protocol)?,
            number("state_schema", version.state_schema)?,
        );
        if protocol == 0 || state_schema == 0 {
            return Err("it has no protocol or state schema (0)".to_owned());
        }
        if min_protocol > protocol {
            return Err(format!(
                "min_protocol {min_protocol} is above protocol {protocol}"
            ));
        }
        Ok(Self {
            protocol,
            min_protocol,
            state_schema,
            profile: version.profile,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

    fn doc(edit: impl Fn(String) -> String) -> Vec<u8> {
        edit(
            r#"{"commit":"0123456789ab","min_protocol":3,"profile":"release","protocol":4,"state_schema":7,"version":"0.1.0"}"#
                .to_owned(),
        )
        .into_bytes()
    }

    fn of(doc: &[u8]) -> Result<Facts, String> {
        let commit = Commit::parse(COMMIT).expect("a commit");
        Facts::of_document(&commit, ReleasePlatform::LinuxX86_64, doc)
    }

    /// Catches a fact read from the wrong field or left at its default: each of the four
    /// facts has its own value in the document, and the platform it names may be absent or
    /// the upload's own.
    ///
    /// Mutant seen red: `min_protocol` taken from `protocol`.
    #[test]
    fn the_facts_are_the_documents_fields() {
        let want = Facts {
            protocol: 4,
            min_protocol: 3,
            state_schema: 7,
            profile: "release".to_owned(),
        };
        assert_eq!(of(&doc(|d| d)), Ok(want.clone()));
        let named = doc(|d| d.replace('}', ",\"platform\":\"linux-x86_64\"}"));
        assert_eq!(of(&named), Ok(want));
    }

    /// Catches the store refusing a release because its report carries a field this server's
    /// `Version` does not name (a newer kbf adds fields): the facts are the same with or
    /// without it. A malformed or contradicting document is still refused (the test below).
    ///
    /// Mutant seen red: `deny_unknown_fields` on `kbf_types::Version`.
    #[test]
    fn a_field_a_newer_kbf_adds_does_not_change_the_facts() {
        let plain = of(&doc(|d| d)).expect("plain");
        let newer = doc(|d| d.replace('}', ",\"future\":{\"a\":[1,2]}}"));
        assert_eq!(of(&newer), Ok(plain));
    }

    /// Catches a document taken that the store or the node must not take, each with its own
    /// reason: text that is not a report, another commit, another platform, a debug build,
    /// zeros, a number past 32 bits and a minimum above the protocol. A minimum equal to
    /// the protocol and a zero minimum are fine.
    ///
    /// Mutants seen red: the platform comparison dropped; `min_protocol > protocol` made
    /// `>=`; the zero check dropped.
    #[test]
    fn a_document_that_contradicts_the_upload_or_the_node_is_refused() {
        let refused = |edit: fn(String) -> String| of(&doc(edit)).expect_err("refused");
        assert!(
            of(b"kbf 0.1.0")
                .expect_err("text")
                .contains("no JSON object")
        );
        assert_eq!(
            refused(|d| d.replace("0123456789ab", "fedcba987654")),
            "it says commit fedcba987654, and the release is 0123456789ab"
        );
        assert_eq!(
            refused(|d| d.replace('}', ",\"platform\":\"darwin-arm64\"}")),
            "it says platform darwin-arm64, and the upload is for linux-x86_64"
        );
        assert_eq!(
            refused(|d| d.replace("\"release\"", "\"debug\"")),
            "it says profile \"debug\", not \"release\""
        );
        assert_eq!(
            refused(|d| d.replace("\"protocol\":4", "\"protocol\":0")),
            "it has no protocol or state schema (0)"
        );
        assert_eq!(
            refused(|d| d.replace("\"state_schema\":7", "\"state_schema\":0")),
            "it has no protocol or state schema (0)"
        );
        assert_eq!(
            refused(|d| d.replace("\"protocol\":4", "\"protocol\":4294967296")),
            "protocol 4294967296 does not fit 32 bits"
        );
        assert_eq!(
            refused(|d| d.replace("\"min_protocol\":3", "\"min_protocol\":4294967296")),
            "min_protocol 4294967296 does not fit 32 bits"
        );
        assert_eq!(
            refused(|d| d.replace("\"state_schema\":7", "\"state_schema\":4294967296")),
            "state_schema 4294967296 does not fit 32 bits"
        );
        assert_eq!(
            refused(|d| d.replace("\"min_protocol\":3", "\"min_protocol\":5")),
            "min_protocol 5 is above protocol 4"
        );
        assert!(
            of(&doc(
                |d| d.replace("\"min_protocol\":3", "\"min_protocol\":4")
            ))
            .is_ok()
        );
        assert!(
            of(&doc(
                |d| d.replace("\"min_protocol\":3", "\"min_protocol\":0")
            ))
            .is_ok()
        );
    }
}
