//! `release notes`: the notes of a release, from the commits since the previous release
//! and the gamma verdict that was checked (docs/design/kbf-cicd.md, section 8.1, item 3).
//! A dry run shows them as an artifact of the verify job before anyone approves.

use kbf_gate::{Floor, ResultDoc, Verdict};

use super::git;
use super::verdict::{Subject, verify};
use crate::Error;

/// Commits listed at most; the rest are counted.
const MAX_COMMITS: usize = 200;

/// The release before `tag` in version order, if any.
fn previous(tags: &[String], tag: &str) -> Option<String> {
    let at = tags.iter().position(|t| t == tag)?;
    at.checked_sub(1).map(|i| tags[i].clone())
}

/// The notes of `s`: refused, like `release verdict`, when the verdict does not pass.
pub fn notes(s: &Subject) -> Result<String, Error> {
    let verified = verify(s)?;
    let tags = git::release_tags(s.repo_dir)?;
    let since = previous(&tags, s.tag);
    let commits = git::commits(s.repo_dir, since.as_deref(), s.commit)?;
    Ok(render(&verified.doc, s.tag, since.as_deref(), &commits))
}

fn section(out: &mut String, title: &str, items: &[String], none: &str) {
    out.push_str(&format!("\n## {title}\n\n"));
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

/// The notes for a verified `doc`.
fn render(doc: &ResultDoc, tag: &str, since: Option<&str>, commits: &[String]) -> String {
    let mut out = format!("# {tag}\n");
    let shown: Vec<String> = commits.iter().take(MAX_COMMITS).cloned().collect();
    let title = match since {
        Some(prev) => format!("Commits since {prev}"),
        None => "Commits (the first release)".to_owned(),
    };
    section(&mut out, &title, &shown, "None.");
    if commits.len() > MAX_COMMITS {
        out.push_str(&format!("- and {} more\n", commits.len() - MAX_COMMITS));
    }

    let mut gamma = Vec::new();
    if let Some(secs) = doc.bake.and_then(|b| Some(b.end?.saturating_sub(b.start))) {
        gamma.push(format!("bake: {}h {:02}m", secs / 3600, secs % 3600 / 60));
    }
    if let Some(l) = doc.load {
        gamma.push(format!(
            "load: {}, {}, {}",
            floor("actions", l.actions),
            floor("builds", l.builds),
            floor("hours", l.hours)
        ));
    }
    for g in &doc.gates {
        let measured = g.measured.map_or("absent".to_owned(), |m| m.to_string());
        let mark = if g.verdict == Verdict::Warn {
            " (warning)"
        } else {
            ""
        };
        gamma.push(format!("gate {}: {measured}{mark}", g.name));
    }
    for w in &doc.waivers {
        gamma.push(format!("waived {} by {}: {}", w.what, w.by, w.reason));
    }
    section(&mut out, "Gamma verdict", &gamma, "No detail recorded.");
    section(
        &mut out,
        "Not reversible",
        &doc.not_reversible,
        "Nothing in this release is flagged as not reversible.",
    );
    out.push_str(&format!(
        "\n## Gamma escapes\n\n{} scenario(s) only gamma found.\n",
        doc.escapes.len()
    ));
    for e in &doc.escapes {
        out.push_str(&format!("- {e}\n"));
    }
    section(
        &mut out,
        "Withdrawn",
        &doc.withdrawn,
        "No release is withdrawn.",
    );
    out
}
