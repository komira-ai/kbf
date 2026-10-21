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

/// What `--version --json` prints. A field this type does not name is refused, and so is
/// one named twice or a required one missing. `platform` (the installer derives it from
/// the binary's header), `admin_protocol`, `meta_log` and `seq` (builds before them lack
/// them) may be absent and are then `None`; `meta_log: None` means not reversible.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Version {
    /// The version of the admin calls between a proxy and the frontend processes it
    /// starts. `None` for a build that predates the field.
    #[serde(default)]
    pub admin_protocol: Option<u64>,
    pub commit: String,
    /// The highest metadata-log machine version the build runs. Going from release N to
    /// N-1 is reversible only if `meta_log(N) <= meta_log(N-1)`; `None` (a build that
    /// predates the field) is unknown, and unknown is not reversible.
    #[serde(default)]
    pub meta_log: Option<u64>,
    pub min_protocol: u64,
    pub protocol: u64,
    pub profile: String,
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
    /// The object a binary printed, strictly: at most [`VERSION_CAP`] bytes, one JSON
    /// object of the fields above and nothing after it.
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
    /// (unknown, not refused), and a missing required field, a field of the wrong type, a
    /// field the contract does not list, one named twice, text that is not JSON and
    /// anything after the object are refused.
    ///
    /// Mutant seen red: `deny_unknown_fields` dropped (the extra field is then accepted).
    #[test]
    fn the_version_object_is_read_by_its_fields() {
        let v = Version::parse(json(COMMIT, "release").as_bytes()).expect("parses");
        assert_eq!(
            v,
            Version {
                admin_protocol: Some(1),
                meta_log: Some(4),
                commit: COMMIT[..12].to_owned(),
                min_protocol: 3,
                protocol: 4,
                profile: "release".to_owned(),
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
        let more = json(COMMIT, "release").replace('}', ",\"future\":true}");
        let twice = json(COMMIT, "release").replace('}', ",\"protocol\":4}");
        let after = format!("{}{{}}", json(COMMIT, "release"));
        for bad in [
            more.as_str(),
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

    /// Catches the `seq` that `kbf --version --json` prints refused as a field the contract
    /// does not list, and a `seq` read as something else than the number: 0 ("could not
    /// count") is a value, not an absent field.
    ///
    /// Mutant to plant: the `seq` field dropped from `Version`.
    #[test]
    fn the_version_object_may_carry_seq() {
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
}
