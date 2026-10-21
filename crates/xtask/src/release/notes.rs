//! `release notes`: the notes of a release, from the commits since the previous release
//! and the verdict that was checked, of the stage [`super::verdict::GATE_STAGE`]
//! (docs/design/kbf-cicd.md, section 8.1, item 3). A dry run shows them as an artifact of
//! the verify job. What is not reversible comes from the two builds, for every stage, never
//! from the verdict.

use std::path::Path;

use kbf_gate::{Floor, ResultDoc, Verdict, reversible};
use serde_json::Value;

use super::git;
use super::offer::{identity_of, whole_field};
use super::verdict::{Subject, bake_secs, verify};
use crate::Error;

/// Commits listed at most; the rest are counted.
const MAX_COMMITS: usize = 200;

/// The `kbf --version --json` of the release being made and of the one before it, which
/// the caller ran from attested tarballs only.
pub struct Identities<'a> {
    /// This release's identity.
    pub current: &'a Path,
    /// The previous release's: required exactly when there is a previous release.
    pub previous: Option<&'a Path>,
}

/// The release before `tag`: the nearest earlier `v*` tag (in version order) that is
/// stable (no `-` suffix) and an ancestor of `commit`. A release candidate, a withdrawn
/// side branch and a tag that sorts before by name only are not "the release before".
pub fn previous_release(repo: &Path, tag: &str, commit: &str) -> Result<Option<String>, Error> {
    let tags = git::release_tags(repo)?;
    // A tag `v*` does not list has no place in the order: a first release.
    let Some(at) = tags.iter().position(|t| t == tag) else {
        return Ok(None);
    };
    let before = &tags[..at];
    for t in before.iter().rev().filter(|t| !t.contains('-')) {
        if git::is_ancestor(repo, &format!("refs/tags/{t}"), commit)? {
            return Ok(Some(t.clone()));
        }
    }
    Ok(None)
}

/// The notes of `s`: refused, like `release verdict`, when the verdict does not pass, and
/// when what is not reversible cannot be determined (fail closed).
pub fn notes(s: &Subject, ids: &Identities) -> Result<String, Error> {
    let verified = verify(s)?;
    let since = previous_release(s.repo_dir, s.tag, s.commit)?;
    let current = identity_of(ids.current, s.commit, "--version-json").map_err(Error::Failed)?;
    let previous = match (&since, ids.previous) {
        (None, None) => None,
        (None, Some(_)) => {
            return Err(Error::Usage(
                "--previous-version-json: there is no previous release".to_owned(),
            ));
        }
        (Some(prev), None) => {
            return Err(Error::Failed(format!(
                "release {prev} precedes {}: --previous-version-json is required, since \
                 what is not reversible cannot be determined without it",
                s.tag
            )));
        }
        (Some(prev), Some(path)) => {
            let commit = git::commit_of(s.repo_dir, prev)?;
            Some(identity_of(path, &commit, "--previous-version-json").map_err(Error::Failed)?)
        }
    };
    let found = irreversible(since.as_deref(), &current, previous.as_ref())?;
    let commits = git::commits(s.repo_dir, since.as_deref(), s.commit)?;
    Ok(render(
        &verified.doc,
        s.tag,
        since.as_deref(),
        &commits,
        &found,
    ))
}

/// What the two builds' identities say makes going back to `since` not possible or not
/// plain, one line each. Unknown counts as not reversible.
fn irreversible(
    since: Option<&str>,
    current: &Value,
    previous: Option<&Value>,
) -> Result<Vec<String>, Error> {
    let (Some(since), Some(previous)) = (since, previous) else {
        return Ok(vec![
            "first release: there is no earlier release to go back to".to_owned(),
        ]);
    };
    let field = |v: &Value, key: &str| whole_field(v, key).map_err(Error::Failed);
    let mut found = Vec::new();
    let (n, p) = (field(current, "meta_log")?, field(previous, "meta_log")?);
    if !reversible(n, p) {
        found.push(match (n, p) {
            (Some(n), Some(p)) => format!(
                "meta_log raised from {p} ({since}) to {n}: {since} cannot open the metadata log \
                 this release writes, so there is no going back to it"
            ),
            _ => format!(
                "meta_log is not stated by this release or by {since}: unknown counts as not \
                 reversible"
            ),
        });
    }
    match (field(current, "protocol")?, field(previous, "protocol")?) {
        (Some(n), Some(p)) if n == p => {}
        (Some(n), Some(p)) => found.push(format!(
            "wire protocol {p} ({since}) to {n}: nodes and proxies speak different protocols \
             during a rollout or a rollback; follow the order of docs/design/kbf-cicd.md, \
             section 8.3"
        )),
        _ => found.push(format!(
            "wire protocol is not stated by this release or by {since}: unknown counts as \
             changed (docs/design/kbf-cicd.md, section 8.3)"
        )),
    }
    Ok(found)
}

fn section(out: &mut String, heading: &str, items: &[String], none: &str) {
    out.push_str(&format!("\n{heading}\n\n"));
    if items.is_empty() {
        out.push_str(&format!("{none}\n"));
    }
    for item in items {
        out.push_str(&format!("- {item}\n"));
    }
}

fn floor(name: &str, f: Floor) -> String {
    format!("{name} {} (needs {})", f.measured, f.required)
}

/// `bake: 24h 00m`. A Gamma verdict has a finished, forward bake; a Beta one has none.
fn bake_line(doc: &ResultDoc) -> Option<String> {
    bake_secs(doc)
        .flatten()
        .map(|secs| format!("bake: {}h {:02}m", secs / 3600, secs % 3600 / 60))
}

/// The verdict in one line, for the top of the notes.
fn summary(doc: &ResultDoc) -> String {
    let warned = doc
        .gates
        .iter()
        .filter(|g| g.verdict == Verdict::Warn)
        .count();
    let mut parts: Vec<String> = bake_line(doc).into_iter().collect();
    parts.push(format!("{} gate(s), {warned} warning(s)", doc.gates.len()));
    format!(
        "green for the set it names; {}; detail under {:?} verdict",
        parts.join(", "),
        doc.stage
    )
}

/// The notes for a verified `doc`. What the approver must read before the click comes first.
fn render(
    doc: &ResultDoc,
    tag: &str,
    since: Option<&str>,
    commits: &[String],
    found: &[String],
) -> String {
    let mut out = format!("# {tag}\n\n## Before you click\n");
    // Only what verify computed from the two builds (`found`). The verdict's own
    // `not_reversible` list is deliberately NOT read: a list typed into a tag is not a
    // source (CEO ruling), so verify is the single source of this section.
    section(
        &mut out,
        "### Not reversible",
        found,
        "Nothing in this release is flagged as not reversible: the metadata log and the wire \
         protocol are the previous release's.",
    );
    let waivers: Vec<String> = doc
        .waivers
        .iter()
        .map(|w| format!("{} waived by {}: {}", w.what, w.by, w.reason))
        .collect();
    section(
        &mut out,
        "### Waivers",
        &waivers,
        "No gate or condition is waived.",
    );
    section(&mut out, "### Verdict", &[summary(doc)], "");
    let shown: Vec<String> = commits.iter().take(MAX_COMMITS).cloned().collect();
    let title = match since {
        Some(prev) => format!("## Commits since {prev}"),
        None => "## Commits (the first release)".to_owned(),
    };
    section(&mut out, &title, &shown, "None.");
    if commits.len() > MAX_COMMITS {
        out.push_str(&format!("- and {} more\n", commits.len() - MAX_COMMITS));
    }

    let mut detail = Vec::new();
    detail.extend(bake_line(doc));
    detail.extend(doc.load.map(|l| {
        format!(
            "load: {}, {}, {}",
            floor("actions", l.actions),
            floor("builds", l.builds),
            floor("hours", l.hours)
        )
    }));
    for g in &doc.gates {
        let measured = g.measured.map_or("absent".to_owned(), |m| m.to_string());
        let mark = if g.verdict == Verdict::Warn {
            " (warning)"
        } else {
            ""
        };
        detail.push(format!("gate {}: {measured}{mark}", g.name));
    }
    for w in &doc.waivers {
        detail.push(format!("waived {} by {}: {}", w.what, w.by, w.reason));
    }
    let title = format!("## {:?} verdict", doc.stage);
    section(&mut out, &title, &detail, "No detail recorded.");
    out.push_str(&format!(
        "\n## {:?} escapes\n\n{} scenario(s) only gamma found.\n",
        doc.stage,
        doc.escapes.len()
    ));
    for e in &doc.escapes {
        out.push_str(&format!("- {e}\n"));
    }
    section(
        &mut out,
        "## Withdrawn",
        &doc.withdrawn,
        "No release is withdrawn.",
    );
    out
}
