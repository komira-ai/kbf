//! The job table of `docs/design/gamma-cicd.md` names exactly the jobs of
//! `.github/workflows/release.yml`.
//!
//! The design walks the release path job by job. A job renamed, added or removed in the
//! workflow without the doc following it leaves the doc describing a path that no longer
//! exists, so [`problems`] compares the two and reports every difference:
//!
//! - **The table is there once.** The doc holds the line [`BEGIN`] and later the line
//!   [`END`], each exactly once; the job table is the table between them.
//! - **Every row names one job.** Each table row between the markers (a line starting `|`,
//!   other than the header separator) has a first cell that is one job id in backticks,
//!   and nothing else. The header row is the one whose first cell is `job`.
//! - **Nothing hidden.** No line between the markers opens a code fence or an HTML comment,
//!   or is indented as code ([`hiding`]): a row there would count here and not render, so a
//!   job could be missing from the rendered table while the lint passed.
//! - **No job twice.** A job id appears in at most one row.
//! - **Nothing the workflow lacks.** Every job the doc names is a job of `release.yml`.
//! - **Nothing the doc omits.** Every job of `release.yml` ([`workflows::job_names`]) has a row.
//! - **Not empty.** A list with no rows fails, so the lint cannot pass on a doc that lost
//!   its table.
//!
//! What it does not check: that the other cells of a row (its needs, its token and rights, its
//! prose) are still true of its job. A reviewer of a
//! change to `release.yml` reads the row the lint points at.

use super::workflows;

#[cfg(test)]
mod tests;

/// The design document the lint reads, relative to the repository root.
pub const DOC: &str = "docs/design/gamma-cicd.md";

/// The workflow whose jobs the document lists.
pub const RELEASE: &str = ".github/workflows/release.yml";

/// The line before the job table.
pub const BEGIN: &str = "<!-- release-jobs: begin -->";

/// The line after the job table.
pub const END: &str = "<!-- release-jobs: end -->";

/// The job ids the job table of `doc` names, in order, or a problem for each malformed part.
pub fn listed_jobs(doc: &str) -> Result<Vec<String>, Vec<String>> {
    let lines: Vec<&str> = doc.lines().collect();
    let at = |marker: &str| -> Vec<usize> {
        lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.trim() == marker)
            .map(|(n, _)| n)
            .collect()
    };
    let (begin, end) = (at(BEGIN), at(END));
    let (&[b], &[e]) = (begin.as_slice(), end.as_slice()) else {
        return Err(vec![format!(
            "{DOC}: the job table must sit between one `{BEGIN}` line and one `{END}` line (found {} and {})",
            begin.len(),
            end.len()
        )]);
    };
    if e < b {
        return Err(vec![format!("{DOC}: `{END}` comes before `{BEGIN}`")]);
    }
    let mut jobs = Vec::new();
    let mut bad = Vec::new();
    for (n, raw) in lines.iter().enumerate().take(e).skip(b + 1) {
        if let Some(what) = hiding(raw) {
            bad.push(format!(
                "{DOC}:{}: {what} inside the job table: a row there would not render, so the table holds none",
                n + 1
            ));
            continue;
        }
        let line = raw.trim();
        if !line.starts_with('|') {
            continue;
        }
        let first = line[1..].split('|').next().unwrap_or("").trim();
        if first == "job" || (!first.is_empty() && first.chars().all(|c| c == '-' || c == ':')) {
            continue;
        }
        match first
            .strip_prefix('`')
            .and_then(|s| s.strip_suffix('`'))
            .filter(|id| !id.is_empty() && !id.contains('`') && !id.contains(char::is_whitespace))
        {
            Some(id) => jobs.push(id.to_owned()),
            None => bad.push(format!(
                "{DOC}:{}: a job row must start with one job id in backticks, not \"{first}\"",
                n + 1
            )),
        }
    }
    if bad.is_empty() { Ok(jobs) } else { Err(bad) }
}

/// What a line between the markers opens or is, when Markdown would not render a table row on
/// it or after it: a code fence, an HTML comment, or a line of an indented code block (four
/// spaces or a tab before any text). `None` for a line that is prose, blank or a table row.
pub fn hiding(line: &str) -> Option<&'static str> {
    let text = line.trim_start();
    let indent = &line[..line.len() - text.len()];
    if text.is_empty() {
        None
    } else if indent.contains('\t') || indent.len() >= 4 {
        Some("an indented code block")
    } else if text.starts_with("```") || text.starts_with("~~~") {
        Some("a code fence")
    } else if line.contains("<!--") || line.contains("-->") {
        Some("an HTML comment")
    } else {
        None
    }
}

/// Every way the job table of `doc` differs from the jobs of the workflow text `release`.
pub fn problems(doc: &str, release: &str) -> Vec<String> {
    let listed = match listed_jobs(doc) {
        Ok(l) => l,
        Err(p) => return p,
    };
    let jobs = match workflows::job_names(release) {
        Ok(j) => j,
        Err(e) => return vec![format!("{RELEASE}: {e}")],
    };
    let mut out = Vec::new();
    if listed.is_empty() {
        out.push(format!("{DOC}: the job table has no rows"));
    }
    for (n, id) in listed.iter().enumerate() {
        if listed[..n].contains(id) {
            out.push(format!("{DOC}: job `{id}` is listed twice"));
        }
        if !jobs.contains(id) {
            out.push(format!(
                "{DOC}: the job table names job `{id}`, which {RELEASE} does not have"
            ));
        }
    }
    for id in &jobs {
        if !listed.contains(id) {
            out.push(format!(
                "{DOC}: {RELEASE} has job `{id}`, which the job table omits"
            ));
        }
    }
    out
}
