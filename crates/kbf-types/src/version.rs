//! What `kbf --version --json` prints, and the one parser of it.
//!
//! The object is the build's own report (`crates/kbf-cli/src/version.rs` writes it). Two
//! readers hold the same bytes: the installer, which runs the staged binary and reads what it
//! prints, and the release store, which keeps the same bytes per platform as the release's
//! version facts (`kbf-<commit>-<platform>.version.json`) and offers them to nodes. They read
//! them with [`Version::parse`], so a document one accepts the other accepts.

use serde::Deserialize;

/// The most a binary's `--version --json` may print.
pub const VERSION_CAP: usize = 4096;

/// Why a `--version --json` report is refused.
#[derive(Debug, thiserror::Error)]
pub enum VersionFault {
    #[error("it did not exit 0 (status {0:?})")]
    Failed(Option<i32>),
    #[error("it printed no JSON object of the fields a build reports: {0}")]
    Unreadable(String),
    #[error("it says commit {got}, and the release is {want}")]
    Commit { want: String, got: String },
    #[error("it says profile {0:?}, not \"release\"")]
    Profile(String),
    #[error("it printed more than {cap} bytes")]
    Oversize { cap: usize },
}

impl VersionFault {
    /// The short word a refusal is recorded under.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            VersionFault::Failed(_) => "version-failed",
            VersionFault::Unreadable(_) => "version-unreadable",
            VersionFault::Commit { .. } => "version-commit",
            VersionFault::Profile(_) => "version-profile",
            VersionFault::Oversize { .. } => "version-oversize",
        }
    }
}

/// What `--version --json` prints. A field this type does not name is ignored: a newer kbf
/// adds fields, and an installer that refused them would be unable to stage any build after
/// it (adding `seq` did that). A field named twice, a required one missing and a wrong type
/// are still refused (serde's derive rejects a repeated known field). `platform` (the
/// installer derives it from the binary's header), `admin_protocol`, `commit_full`, `meta_log`,
/// `relay` and `seq` (builds before them lack them) may be absent and are then `None`;
/// `meta_log: None` means not reversible. `commit_full` is also absent from a build that does
/// not know its full commit.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Version {
    /// The version of the admin calls between a proxy and the frontend processes it
    /// starts. `None` for a build that predates the field.
    #[serde(default)]
    pub admin_protocol: Option<u64>,
    pub commit: String,
    /// The 40 hex digits of the commit built, when the build knows them. `None` for a build
    /// that predates the field or did not know its full commit (no git checkout).
    #[serde(default)]
    pub commit_full: Option<String>,
    /// The highest metadata-log machine version the build runs. Going from release N to
    /// N-1 is reversible only if `meta_log(N) <= meta_log(N-1)`; `None` (a build that
    /// predates the field) is unknown, and unknown is not reversible.
    #[serde(default)]
    pub meta_log: Option<u64>,
    pub min_protocol: u64,
    pub protocol: u64,
    pub profile: String,
    /// How far this build carries what the installer reports to the frontend: at least 1 when
    /// it reads `available` and `release_source` from `status.json` and sends them on. `None`
    /// for a build that predates the field, which relays nothing of the sort. The level a
    /// host needs is the installer's (`kbf_installer::binary::RELAY_NEEDED`), not this type's.
    #[serde(default)]
    pub relay: Option<u64>,
    /// The number of commits behind the build's commit, which puts two builds of one branch
    /// in order; 0 when the build could not count them (a shallow checkout), which is below
    /// every counted build. `None` for a build that predates the field.
    #[serde(default)]
    pub seq: Option<u64>,
    pub state_schema: u64,
    pub version: String,
    /// `linux-x86_64`, `linux-arm64` or `darwin-arm64`, when the build says.
    #[serde(default)]
    pub platform: Option<String>,
}

impl Version {
    /// The object a binary printed: at most [`VERSION_CAP`] bytes, one JSON object holding
    /// the fields above (others are ignored) and nothing after it.
    ///
    /// # Errors
    /// [`VersionFault::Oversize`] or [`VersionFault::Unreadable`].
    pub fn parse(stdout: &[u8]) -> Result<Version, VersionFault> {
        if stdout.len() > VERSION_CAP {
            return Err(VersionFault::Oversize { cap: VERSION_CAP });
        }
        serde_json::from_slice(stdout).map_err(|e| VersionFault::Unreadable(e.to_string()))
    }

    /// Whether this build is `commit`'s release build: it names the commit's first 12 hex
    /// digits and is a `release` profile, not a debug build.
    ///
    /// # Errors
    /// [`VersionFault::Commit`] or [`VersionFault::Profile`].
    pub fn check(&self, commit: impl AsRef<str>) -> Result<(), VersionFault> {
        let commit = commit.as_ref();
        let want = commit.get(..12).unwrap_or(commit);
        if self.commit != want {
            return Err(VersionFault::Commit {
                want: want.to_owned(),
                got: self.commit.clone(),
            });
        }
        if self.profile != "release" {
            return Err(VersionFault::Profile(self.profile.clone()));
        }
        Ok(())
    }
}

/// The newest of each version number an installer understands: a release that declares more
/// is refused (`installer_too_old`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Supported {
    pub protocol: u64,
    pub min_protocol: u64,
    pub state_schema: u64,
}

/// A number a release declares that is past what an installer understands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{field} {got}; this installer understands up to {most}")]
pub struct TooNew {
    /// The number: `protocol`, `min_protocol` or `state_schema`.
    pub field: &'static str,
    /// What the release declares.
    pub got: u64,
    /// The most the installer understands.
    pub most: u64,
}

/// Whether an installer that understands `supported` may take a component that declared
/// `declared`: `Err` when any of its numbers is past what the installer understands, the first
/// in the order `protocol`, `min_protocol`, `state_schema` (the rule behind upgrading the
/// installer before a kbf that raises one).
///
/// # Errors
/// [`TooNew`], naming the number.
pub fn understood(supported: Supported, declared: Supported) -> Result<(), TooNew> {
    let newer = [
        ("protocol", declared.protocol, supported.protocol),
        (
            "min_protocol",
            declared.min_protocol,
            supported.min_protocol,
        ),
        (
            "state_schema",
            declared.state_schema,
            supported.state_schema,
        ),
    ];
    for (field, got, most) in newer {
        if got > most {
            return Err(TooNew { field, got, most });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
    const OTHER: &str = "fedcba9876543210fedcba9876543210fedcba98";

    fn json(commit: &str, profile: &str) -> String {
        format!(
            "{{\"admin_protocol\":1,\"commit\":\"{}\",\"meta_log\":4,\"min_protocol\":3,\
             \"profile\":\"{profile}\",\"protocol\":4,\"state_schema\":7,\"version\":\"0.1.0\"}}",
            &commit[..12]
        )
    }

    /// Catches a field of the object read wrongly or an optional one required: the fields
    /// are read by name, builds before `admin_protocol`, `meta_log` and `seq` omit them
    /// (unknown, not refused), and a missing required field, a field of the wrong type, one
    /// named twice, text that is not JSON and anything after the object are refused.
    ///
    /// Mutant to plant: the `seq` field's `#[serde(default)]` dropped (an older build is
    /// then refused).
    #[test]
    fn the_version_object_is_read_by_its_fields() {
        let v = Version::parse(json(COMMIT, "release").as_bytes()).expect("parses");
        assert_eq!(
            v,
            Version {
                admin_protocol: Some(1),
                meta_log: Some(4),
                commit: COMMIT[..12].to_owned(),
                commit_full: None,
                min_protocol: 3,
                protocol: 4,
                profile: "release".to_owned(),
                relay: None,
                seq: None,
                state_schema: 7,
                version: "0.1.0".to_owned(),
                platform: None,
            }
        );
        let old = r#"{"commit":"0123456789ab","min_protocol":3,"profile":"release","protocol":4,"state_schema":7,"version":"0.1.0"}"#;
        let old = Version::parse(old.as_bytes()).expect("an older build parses");
        assert_eq!(
            (old.admin_protocol, old.meta_log, old.seq),
            (None, None, None)
        );
        let said = json(COMMIT, "release").replace('}', ",\"platform\":\"linux-arm64\"}");
        assert_eq!(
            Version::parse(said.as_bytes()).expect("parses").platform,
            Some("linux-arm64".to_owned())
        );
        let twice = json(COMMIT, "release").replace('}', ",\"protocol\":4}");
        let after = format!("{}{{}}", json(COMMIT, "release"));
        for bad in [
            twice.as_str(),
            after.as_str(),
            r#"{"commit":"abc"}"#,
            "kbf 0.1.0",
            "",
            &json(COMMIT, "release").replace("\"protocol\":4", "\"protocol\":\"4\""),
        ] {
            let err = Version::parse(bad.as_bytes()).expect_err(bad);
            assert_eq!(err.code(), "version-unreadable", "{bad}");
        }
    }

    /// Catches an installer that strands on a field a newer kbf adds to `--version --json`: a
    /// field this `Version` does not name is ignored and the named fields still parse. Adding
    /// `seq` was refused by every installer built before it; the next field must not be.
    /// Nested values (an object, an array) in an unknown field are skipped too.
    ///
    /// Mutant seen red: `deny_unknown_fields` on `Version` (the extra field is then refused).
    #[test]
    fn the_version_object_ignores_a_field_it_does_not_know() {
        let shape = json(COMMIT, "release");
        for extra in [
            ",\"future\":true}",
            ",\"future\":{\"a\":[1,2,{\"b\":null}]},\"other\":\"x\"}",
        ] {
            let said = shape.replace('}', extra);
            let v = Version::parse(said.as_bytes()).expect("an unknown field is ignored");
            assert_eq!(v.commit, COMMIT[..12], "{said}");
            assert_eq!((v.protocol, v.min_protocol, v.state_schema), (4, 3, 7));
            assert_eq!((v.admin_protocol, v.meta_log), (Some(1), Some(4)));
            assert_eq!(v.profile, "release");
            assert_eq!(v.version, "0.1.0");
        }
    }

    /// Catches a tolerant reader that stopped checking what it does name: a known field named
    /// twice is refused (also with an unknown field present), and so is a missing required
    /// one.
    ///
    /// Mutant to plant: a hand-written reader that keeps the last of two equal keys (the
    /// second `commit` then wins and the object is accepted).
    #[test]
    fn a_tolerant_version_object_still_refuses_a_repeated_or_missing_field() {
        let shape = json(COMMIT, "release");
        let twice = shape.replace('}', ",\"future\":1,\"commit\":\"0123456789ab\"}");
        let err = Version::parse(twice.as_bytes()).expect_err("commit twice");
        assert_eq!(err.code(), "version-unreadable");
        assert!(err.to_string().contains("duplicate field"), "{err}");
        for required in [
            "commit",
            "min_protocol",
            "profile",
            "protocol",
            "state_schema",
            "version",
        ] {
            let mut object: serde_json::Value =
                serde_json::from_str(&shape).expect("fixture is JSON");
            object["future"] = serde_json::Value::Bool(true);
            object.as_object_mut().expect("an object").remove(required);
            let err = Version::parse(object.to_string().as_bytes()).expect_err(required);
            assert_eq!(err.code(), "version-unreadable", "{required}");
        }
    }

    /// Catches the `seq` that `kbf --version --json` prints (the number of commits behind the
    /// build) not being read, and a `seq` that is read as something else than the number: 0
    /// ("could not count") is a value, not an absent field. Both real shapes parse: the one
    /// a build before `seq` printed and the one a build after prints.
    ///
    /// Mutant to plant: the `seq` field dropped from `Version` (the value is then lost).
    #[test]
    fn the_version_object_may_carry_seq() {
        let before = Version::parse(json(COMMIT, "release").as_bytes()).expect("pre-seq");
        assert_eq!(before.seq, None);
        let said = json(COMMIT, "release").replace('}', ",\"seq\":812}");
        let v = Version::parse(said.as_bytes()).expect("a build with a seq parses");
        assert_eq!(v.seq, Some(812));
        let zero = json(COMMIT, "release").replace('}', ",\"seq\":0}");
        let v = Version::parse(zero.as_bytes()).expect("a build that could not count parses");
        assert_eq!(v.seq, Some(0));
        let text = json(COMMIT, "release").replace('}', ",\"seq\":\"812\"}");
        let err = Version::parse(text.as_bytes()).expect_err("a seq that is text");
        assert_eq!(err.code(), "version-unreadable");
    }

    /// Catches the `relay` that `kbf --version --json` prints (how far the build relays what
    /// the installer reports) not being read, a build that predates it refused instead of
    /// read as `None`, and a level that is text accepted.
    ///
    /// Mutant to plant: the `relay` field dropped from `Version` (the value is then lost).
    #[test]
    fn the_version_object_may_carry_a_relay_level() {
        let before = Version::parse(json(COMMIT, "release").as_bytes()).expect("pre-relay");
        assert_eq!(before.relay, None);
        for level in [0, 1, 2] {
            let said = json(COMMIT, "release").replace('}', &format!(",\"relay\":{level}}}"));
            let v = Version::parse(said.as_bytes()).expect("a build with a relay level parses");
            assert_eq!(v.relay, Some(level));
        }
        let text = json(COMMIT, "release").replace('}', ",\"relay\":\"1\"}");
        let err = Version::parse(text.as_bytes()).expect_err("a relay that is text");
        assert_eq!(err.code(), "version-unreadable");
    }

    /// Catches the `commit_full` that `kbf --version --json` prints (the 40 hex digits of the
    /// build's commit) not being read, a build that predates it or does not know its commit
    /// refused instead of read as `None`, and a value that is not text accepted.
    ///
    /// Mutants to plant: `#[serde(skip)]` or a misspelled `#[serde(rename = "commit_ful")]` on
    /// the field (the value is then lost: red on the `Some(COMMIT)` line); the field typed
    /// `Option<serde_json::Value>` (the number is then accepted: red on the last assertion).
    #[test]
    fn the_version_object_may_carry_the_full_commit() {
        let before = Version::parse(json(COMMIT, "release").as_bytes()).expect("pre-field");
        assert_eq!(before.commit_full, None);
        let extra = format!(",\"commit_full\":\"{COMMIT}\"}}");
        let said = json(COMMIT, "release").replace('}', &extra);
        let v = Version::parse(said.as_bytes()).expect("a build with the full commit parses");
        assert_eq!(v.commit_full.as_deref(), Some(COMMIT));
        assert_eq!(v.commit, COMMIT[..12], "the short commit is read as before");
        let text = json(COMMIT, "release").replace('}', ",\"commit_full\":12}");
        let err = Version::parse(text.as_bytes()).expect_err("a number");
        assert_eq!(err.code(), "version-unreadable");
    }

    /// Catches an unbounded read of what a not yet trusted binary prints: more than 4 KiB
    /// is refused whatever it holds, and exactly 4 KiB of valid JSON (padded with spaces)
    /// is read.
    ///
    /// Mutant seen red: the length check dropped.
    #[test]
    fn a_version_object_is_read_up_to_four_kib() {
        let text = json(COMMIT, "release");
        let fits = format!("{text}{}", " ".repeat(VERSION_CAP - text.len()));
        assert_eq!(fits.len(), VERSION_CAP);
        Version::parse(fits.as_bytes()).expect("4 KiB fits");
        let big = format!("{fits} ");
        assert_eq!(
            Version::parse(big.as_bytes()).expect_err("over").code(),
            "version-oversize"
        );
    }

    /// Catches each check of a version being skipped: the commit (the first 12 digits of
    /// the one named) and the profile (a debug build is not a release). Each is its own
    /// refusal, and a commit shorter than 12 digits is compared whole.
    ///
    /// Mutants seen red: the commit comparison dropped; the profile comparison dropped.
    #[test]
    fn a_version_must_name_the_commit_and_be_a_release_build() {
        let release = Version::parse(json(COMMIT, "release").as_bytes()).expect("parses");
        release.check(COMMIT).expect("the release of the commit");

        let other = Version::parse(json(OTHER, "release").as_bytes()).expect("parses");
        let err = other.check(COMMIT).expect_err("another commit");
        assert_eq!(err.code(), "version-commit");
        assert_eq!(
            err.to_string(),
            format!(
                "it says commit {}, and the release is {}",
                &OTHER[..12],
                &COMMIT[..12]
            )
        );
        let err = release.check("0123").expect_err("a short commit");
        assert_eq!(
            err.to_string(),
            format!("it says commit {}, and the release is 0123", &COMMIT[..12])
        );

        let debug = Version::parse(json(COMMIT, "debug").as_bytes()).expect("parses");
        let err = debug.check(COMMIT).expect_err("debug");
        assert_eq!(err.code(), "version-profile");
    }

    /// Catches a code or a message that changed under a refusal an operator or a record
    /// reads: each fault has its own word.
    #[test]
    fn each_fault_has_its_own_word() {
        assert_eq!(VersionFault::Failed(Some(1)).code(), "version-failed");
        assert_eq!(
            VersionFault::Failed(None).to_string(),
            "it did not exit 0 (status None)"
        );
    }

    fn numbers(protocol: u64, min_protocol: u64, state_schema: u64) -> Supported {
        Supported {
            protocol,
            min_protocol,
            state_schema,
        }
    }

    /// Catches an installer that takes a component declaring more than it understands, in any
    /// one of the three numbers (each tried alone, the others equal), and one that refuses a
    /// component declaring no more (equal, or less: an installer ahead of the kbf it takes is
    /// the order the rule asks for). The refusal names the number and both values.
    ///
    /// Mutants it catches: one of the three comparisons dropped; `>` made `>=`; `got` and
    /// `most` swapped in the refusal; the numbers of `declared` and `supported` swapped.
    #[test]
    fn a_component_is_understood_when_it_declares_no_more_than_the_installer_does() {
        let ceiling = numbers(2, 2, 4);
        for declared in [ceiling, numbers(1, 1, 3), numbers(0, 0, 0)] {
            assert_eq!(understood(ceiling, declared), Ok(()));
        }
        for (declared, field, got, most) in [
            (numbers(3, 2, 4), "protocol", 3, 2),
            (numbers(2, 3, 4), "min_protocol", 3, 2),
            (numbers(2, 2, 5), "state_schema", 5, 4),
        ] {
            let refused = understood(ceiling, declared).expect_err(field);
            let said = (refused.field, refused.got, refused.most);
            assert_eq!(said, (field, got, most));
        }
        let refused = understood(ceiling, numbers(3, 2, 4)).expect_err("protocol");
        assert_eq!(
            refused.to_string(),
            "protocol 3; this installer understands up to 2"
        );
    }

    /// Catches the order of the checks changing under a refusal an operator reads: when more
    /// than one number is past the installer's, the first of `protocol`, `min_protocol`,
    /// `state_schema` is the one named.
    ///
    /// Mutant it catches: the three checks run in another order.
    #[test]
    fn the_first_number_past_the_installer_is_the_one_named() {
        let ceiling = numbers(1, 1, 1);
        let all = understood(ceiling, numbers(2, 2, 2)).expect_err("all three");
        assert_eq!(all.field, "protocol");
        let later = understood(ceiling, numbers(1, 2, 2)).expect_err("two");
        assert_eq!(later.field, "min_protocol");
    }
}
