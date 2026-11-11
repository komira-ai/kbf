//! The `if:` of every job of `release.yml` that publishes to the apt repository, in one place.
//!
//! Two lints read these strings and must agree on them: this one (`workflows`), which lets a job
//! hold `id-token: write` only under one of its exact `if:` spellings, and the release lint
//! (`release_workflow`), which holds each job to its own. The strings are built here, from the
//! release's condition ([`RELEASE_BODY`]) and the attempt a job belongs to, so a clause cannot
//! be dropped from one and kept in the other.
//!
//! Every job is under the release's condition. The first attempt of the suite `testing` carries
//! it as it is; every other job also carries `!cancelled() && !failure()`, so that it runs after
//! a needed job that was skipped (an attempt that was not needed, or a read that did not run) and
//! after nothing that failed or was cancelled, and the clause that says which job it follows.

/// The two suites of the apt repository, in the order a release goes through them.
pub const SUITES: [&str; 2] = ["testing", "stable"];

/// The attempts of each suite: a write that loses the race for the index is followed by another
/// read, index and write, and the third is the last.
pub const ATTEMPTS: [u8; 3] = [1, 2, 3];

/// The release's condition without its `${{ }}`: a successful run of the pipeline for a push to
/// `main` of this repository, of code that is not a change to the pipeline.
pub const RELEASE_BODY: &str = "github.event.workflow_run.conclusion == 'success' && github.event.workflow_run.event == 'push' && github.event.workflow_run.head_branch == 'main' && github.event.workflow_run.head_repository.full_name == github.repository && needs.plan.outputs.code == 'true' && needs.plan.outputs.pipeline != 'true'";

/// The three jobs of a suite and attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Reads the index with the cloud role.
    Read,
    /// Merges the packages into the index and signs it with the key.
    Index,
    /// Writes the packages and the index with the cloud role.
    Write,
}

impl Kind {
    pub const ALL: [Kind; 3] = [Kind::Read, Kind::Index, Kind::Write];

    pub fn word(self) -> &'static str {
        match self {
            Kind::Read => "read",
            Kind::Index => "index",
            Kind::Write => "write",
        }
    }
}

/// The name of a job: `testing-apt-read`, `testing-apt-index-2`, `stable-apt-write-3`.
pub fn job_name(suite: &str, kind: Kind, attempt: u8) -> String {
    let word = kind.word();
    if attempt == 1 {
        format!("{suite}-apt-{word}")
    } else {
        format!("{suite}-apt-{word}-{attempt}")
    }
}

/// The two install checks, one job for each architecture.
pub const CHECKS: [&str; 2] = ["check-apt-amd64", "check-apt-arm64"];

/// `${{ <the release's condition> }}`.
pub fn plain() -> String {
    format!("${{{{ {RELEASE_BODY} }}}}")
}

/// The release's condition after a job that may have been skipped: `extra` says which job it
/// follows.
pub fn after(extra: &str) -> String {
    format!("${{{{ !cancelled() && !failure() && {RELEASE_BODY} && {extra} }}}}")
}

/// What an install check needs to have happened: a write of `testing` that landed, in one of the
/// three attempts.
pub fn check_condition() -> String {
    let landed: Vec<String> = ATTEMPTS
        .iter()
        .map(|&n| {
            format!(
                "needs.{}.outputs.conflict == 'false'",
                job_name("testing", Kind::Write, n)
            )
        })
        .collect();
    after(&format!("({})", landed.join(" || ")))
}

/// What the jobs of `stable` first attempt wait for: both install checks passed.
fn checks_passed() -> String {
    CHECKS
        .iter()
        .map(|c| format!("needs.{c}.result == 'success'"))
        .collect::<Vec<_>>()
        .join(" && ")
}

/// The `if:` of the job `kind` of `suite` in `attempt`.
pub fn condition(suite: &str, kind: Kind, attempt: u8) -> String {
    let result = |k: Kind| format!("needs.{}.result == 'success'", job_name(suite, k, attempt));
    if attempt == 1 {
        return if suite == "testing" {
            plain()
        } else {
            after(&checks_passed())
        };
    }
    match kind {
        Kind::Read => after(&format!(
            "needs.{}.outputs.conflict == 'true'",
            job_name(suite, Kind::Write, attempt - 1)
        )),
        Kind::Index => after(&result(Kind::Read)),
        Kind::Write => after(&format!(
            "{} && {}",
            result(Kind::Read),
            result(Kind::Index)
        )),
    }
}

/// The `if:` of every job that holds `id-token: write` beside gamma: the reads and the writes.
pub fn id_token_conditions() -> Vec<String> {
    let mut all = Vec::new();
    for suite in SUITES {
        for attempt in ATTEMPTS {
            for kind in [Kind::Read, Kind::Write] {
                all.push(condition(suite, kind, attempt));
            }
        }
    }
    all.sort();
    all.dedup();
    all
}
