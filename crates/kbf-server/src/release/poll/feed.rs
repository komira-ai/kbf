//! Reading a GitHub Releases feed: which entries are releases the farm could run.
//!
//! An entry is a candidate when it is a published (not draft, not pre-release) release of a
//! `v*` tag whose notes name the commit and the ref it was built from (the notes
//! `cargo xtask release publish` writes: "built and attested for <commit> on <ref>"), with a
//! `SHA256SUMS` asset and, for every tarball of that commit, its provenance bundle. This
//! only reads names: **no attestation is verified here**, and a candidate is a proposal for
//! an operator, never a run. Each entry that is not a candidate is counted under the first
//! [`Ignore`] reason it meets.

use serde_json::Value;

/// The suffix of a provenance bundle's asset name (`xtask::bundles::SUFFIX`).
const BUNDLE_SUFFIX: &str = ".sigstore.json";

/// The checksum asset every release holds.
const SUMS: &str = "SHA256SUMS";

/// Why an entry of the feed is not a candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ignore {
    /// Not an object, or without a string tag, a publication time or an asset list.
    Malformed,
    /// A draft.
    Draft,
    /// A pre-release.
    Prerelease,
    /// The tag does not start with `v`.
    NotVersionTag,
    /// The notes do not name the commit the release was built from.
    NoCommit,
    /// The notes name a ref that is not an allowed one.
    RefNotAllowed,
    /// No `SHA256SUMS` asset.
    NoChecksums,
    /// No tarball of the commit, or a tarball without its provenance bundle.
    Unattested,
}

impl Ignore {
    /// Every reason, in the order the metrics list them.
    pub const ALL: [Ignore; 8] = [
        Ignore::Malformed,
        Ignore::Draft,
        Ignore::Prerelease,
        Ignore::NotVersionTag,
        Ignore::NoCommit,
        Ignore::RefNotAllowed,
        Ignore::NoChecksums,
        Ignore::Unattested,
    ];

    /// The reason as a metric label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Ignore::Malformed => "malformed",
            Ignore::Draft => "draft",
            Ignore::Prerelease => "prerelease",
            Ignore::NotVersionTag => "not_version_tag",
            Ignore::NoCommit => "no_commit",
            Ignore::RefNotAllowed => "ref_not_allowed",
            Ignore::NoChecksums => "no_checksums",
            Ignore::Unattested => "unattested",
        }
    }

    /// Where the reason sits in [`Ignore::ALL`].
    #[must_use]
    pub fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|r| *r == self)
            .expect("every reason is in ALL")
    }
}

/// A release that could be run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    /// The `v*` tag.
    pub tag: String,
    /// The commit the notes name: 40 lowercase hex digits.
    pub commit: String,
    /// When it was published, in seconds since the Unix epoch.
    pub published_at_secs: i64,
}

/// The entries of a feed body: a JSON array.
///
/// # Errors
/// The body is not a JSON array.
pub fn entries(body: &[u8]) -> Result<Vec<Value>, String> {
    match serde_json::from_slice::<Value>(body) {
        Ok(Value::Array(entries)) => Ok(entries),
        Ok(_) => Err("the feed is not a JSON array".to_owned()),
        Err(e) => Err(format!("the feed is not JSON: {e}")),
    }
}

/// Whether `entry` is a candidate, with `refs` the refs a release may be built from.
///
/// # Errors
/// The reason it is not.
pub fn judge(entry: &Value, refs: &[String]) -> Result<Candidate, Ignore> {
    let tag = entry["tag_name"].as_str().ok_or(Ignore::Malformed)?;
    let published = entry["published_at"].as_str().ok_or(Ignore::Malformed)?;
    let published_at_secs =
        time::OffsetDateTime::parse(published, &time::format_description::well_known::Rfc3339)
            .map_err(|_| Ignore::Malformed)?
            .unix_timestamp();
    let assets = entry["assets"].as_array().ok_or(Ignore::Malformed)?;
    let notes = entry["body"].as_str().unwrap_or_default();
    if entry["draft"].as_bool().unwrap_or(false) {
        return Err(Ignore::Draft);
    }
    if entry["prerelease"].as_bool().unwrap_or(false) {
        return Err(Ignore::Prerelease);
    }
    if !tag.starts_with('v') || tag.len() < 2 {
        return Err(Ignore::NotVersionTag);
    }
    let (commit, built_on) = built_from(notes).ok_or(Ignore::NoCommit)?;
    if !refs.iter().any(|r| r == built_on) {
        return Err(Ignore::RefNotAllowed);
    }
    let names: Vec<&str> = assets.iter().filter_map(|a| a["name"].as_str()).collect();
    if !names.contains(&SUMS) {
        return Err(Ignore::NoChecksums);
    }
    let mark = format!("-{commit}-");
    let tarballs: Vec<&&str> = names
        .iter()
        .filter(|n| n.ends_with(".tar.gz") && n.contains(&mark))
        .collect();
    let paired = tarballs
        .iter()
        .all(|t| names.contains(&format!("{t}{BUNDLE_SUFFIX}").as_str()));
    if tarballs.is_empty() || !paired {
        return Err(Ignore::Unattested);
    }
    Ok(Candidate {
        tag: tag.to_owned(),
        commit: commit.to_owned(),
        published_at_secs,
    })
}

/// The commit and ref in "attested for <40 hex> on <ref> (" of a release's notes.
fn built_from(notes: &str) -> Option<(&str, &str)> {
    let (_, after) = notes.split_once("attested for ")?;
    let (commit, rest) = after.split_at_checked(40)?;
    let hex = |b: u8| b.is_ascii_digit() || (b'a'..=b'f').contains(&b);
    if !commit.bytes().all(hex) {
        return None;
    }
    let on = rest.strip_prefix(" on ")?;
    let built_on = on.split_whitespace().next()?;
    Some((commit, built_on))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const C: &str = "0123456789abcdef0123456789abcdef01234567";

    fn refs() -> Vec<String> {
        vec!["main".to_owned()]
    }

    fn notes(commit: &str, on: &str) -> String {
        format!(
            "The files run 9 of artifacts.yml built and attested for {commit} on {on} (https://x), unchanged"
        )
    }

    fn release() -> Value {
        let tar = format!("kbf-{C}-linux-x86_64.tar.gz");
        json!({
            "tag_name": "v1.2",
            "draft": false,
            "prerelease": false,
            "published_at": "2026-10-20T01:02:03Z",
            "body": notes(C, "main"),
            "assets": [
                {"name": "SHA256SUMS"},
                {"name": tar},
                {"name": format!("{tar}.sigstore.json")},
            ],
        })
    }

    fn with(key: &str, value: Value) -> Value {
        let mut r = release();
        r[key] = value;
        r
    }

    /// Catches a well-formed release that is turned away, and its fields read wrongly.
    #[test]
    fn a_published_attested_release_is_a_candidate() {
        let got = judge(&release(), &refs()).expect("candidate");
        assert_eq!(got.tag, "v1.2");
        assert_eq!(got.commit, C);
        assert_eq!(got.published_at_secs, 1_792_458_123);
    }

    /// Catches each reason an entry is ignored being dropped, reordered or mislabelled:
    /// every defect planted alone in an otherwise good release.
    #[test]
    fn each_defect_is_ignored_under_its_own_reason() {
        let tar = format!("kbf-{C}-linux-x86_64.tar.gz");
        let cases = [
            (json!("not an object"), Ignore::Malformed),
            (with("tag_name", json!(7)), Ignore::Malformed),
            (with("published_at", json!("yesterday")), Ignore::Malformed),
            (with("published_at", Value::Null), Ignore::Malformed),
            (with("assets", Value::Null), Ignore::Malformed),
            (with("draft", json!(true)), Ignore::Draft),
            (with("prerelease", json!(true)), Ignore::Prerelease),
            (with("tag_name", json!("nightly")), Ignore::NotVersionTag),
            (with("tag_name", json!("v")), Ignore::NotVersionTag),
            (with("body", json!("no commit here")), Ignore::NoCommit),
            (with("body", Value::Null), Ignore::NoCommit),
            (
                with("body", json!(notes("ABCDEF", "main"))),
                Ignore::NoCommit,
            ),
            (
                with("body", json!(notes(&C.to_uppercase(), "main"))),
                Ignore::NoCommit,
            ),
            (
                with("body", json!(format!("attested for {C}"))),
                Ignore::NoCommit,
            ),
            (
                with("body", json!(format!("attested for {C} on "))),
                Ignore::NoCommit,
            ),
            (
                with("body", json!(notes(C, "feature"))),
                Ignore::RefNotAllowed,
            ),
            (
                with(
                    "assets",
                    json!([{"name": tar}, {"name": format!("{tar}.sigstore.json")}]),
                ),
                Ignore::NoChecksums,
            ),
            (
                with("assets", json!([{"name": "SHA256SUMS"}])),
                Ignore::Unattested,
            ),
            (
                with("assets", json!([{"name": "SHA256SUMS"}, {"name": tar}])),
                Ignore::Unattested,
            ),
            (
                with(
                    "assets",
                    json!([{"name": "SHA256SUMS"}, {"name": format!("kbf-{}-linux-x86_64.tar.gz", "f".repeat(40)) }]),
                ),
                Ignore::Unattested,
            ),
        ];
        for (entry, want) in cases {
            assert_eq!(judge(&entry, &refs()), Err(want), "{entry}");
        }
    }

    /// Catches the allowed refs being ignored: the same release is a candidate when its ref
    /// is listed.
    #[test]
    fn the_allowed_refs_decide() {
        let r = with("body", json!(notes(C, "release")));
        assert_eq!(judge(&r, &refs()), Err(Ignore::RefNotAllowed));
        let both = vec!["main".to_owned(), "release".to_owned()];
        assert!(judge(&r, &both).is_ok());
    }

    /// Catches a tarball with a bundle for another file passing, and a second tarball
    /// without its bundle hiding behind the first.
    #[test]
    fn every_tarball_of_the_commit_needs_its_own_bundle() {
        let a = format!("kbf-{C}-linux-x86_64.tar.gz");
        let b = format!("kbf-{C}-linux-arm64.tar.gz");
        let r = with(
            "assets",
            json!([
                {"name": "SHA256SUMS"}, {"name": a}, {"name": format!("{a}.sigstore.json")},
                {"name": b},
            ]),
        );
        assert_eq!(judge(&r, &refs()), Err(Ignore::Unattested));
    }

    /// Catches the labels colliding or the index disagreeing with the list the metrics use.
    #[test]
    fn reasons_have_distinct_labels_at_their_index() {
        let mut seen = std::collections::BTreeSet::new();
        for (i, r) in Ignore::ALL.iter().enumerate() {
            assert_eq!(r.index(), i);
            assert!(seen.insert(r.label()), "{} twice", r.label());
        }
    }

    /// Catches a body that is not a list being read as an empty feed.
    #[test]
    fn a_feed_is_a_json_array() {
        assert_eq!(entries(b"[1, 2]").expect("array").len(), 2);
        assert!(entries(b"{}").is_err());
        assert!(entries(b"<html>").is_err());
    }
}
