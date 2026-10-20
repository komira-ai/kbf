//! The result document, `beta-result.json` (docs/design/kbf-cicd.md, sections 3 and 10),
//! and the job summary's gate table.
//!
//! The file is written `RUNNING` first and a finished status on every exit: [`Writer`]'s
//! guard writes `FAILED` if the scenario ends (a panic, an early return) without having
//! finished it. A run that is cancelled or killed leaves `RUNNING`, which a reader takes
//! for "not run", never for "passed". The smoke is not an attested run, and the document
//! says so.

use std::path::{Path, PathBuf};

use serde::Serialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::farm::ProcRecord;
use crate::gates::{Gate, Verdict};

/// The stage the document belongs to (`release/stages.yml`).
pub const STAGE: &str = "beta";

/// The document's layout, counted up when a field changes meaning, so a reader (the offer to
/// gamma, a dashboard) can refuse a document it does not know.
pub const SCHEMA_VERSION: u32 = 1;

/// The variable that names the commit of the harness itself, when it is not the commit
/// under test (the attested beta runs the released bytes with the harness of a checkout).
pub const HARNESS_COMMIT_VAR: &str = "KBF_HARNESS_COMMIT";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Status {
    #[serde(rename = "RUNNING")]
    Running,
    #[serde(rename = "PASSED")]
    Passed,
    #[serde(rename = "FAILED")]
    Failed,
    #[serde(rename = "TIMED_OUT")]
    TimedOut,
}

impl Status {
    /// The status as the document spells it (`PASSED`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Running => "RUNNING",
            Status::Passed => "PASSED",
            Status::Failed => "FAILED",
            Status::TimedOut => "TIMED_OUT",
        }
    }
}

/// What the run used, so the document says what was and was not exercised.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Runtime {
    /// `native` or `stand-in container`.
    pub driver: String,
    /// What containers ran: `none (native driver)` when no container path is exercised.
    pub container: String,
    /// `buck2 --version`'s first line.
    pub buck2_version: String,
}

/// What the builds did, in numbers.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Builds {
    pub run: u64,
    pub failed: u64,
    pub concurrency: usize,
    /// Builds per kind (`compile`, `pause`, `upload`).
    pub by_kind: std::collections::BTreeMap<String, u64>,
}

/// `beta-result.json`.
#[derive(Clone, Debug, Serialize)]
pub struct Document {
    /// [`SCHEMA_VERSION`].
    pub schema_version: u32,
    pub stage: &'static str,
    pub scenario: String,
    pub commit: String,
    /// The release set's hash: none in the smoke, which runs on its own binaries.
    pub set_hash: Option<String>,
    pub harness_commit: String,
    pub run_id: String,
    pub attempt: String,
    /// Whether the bytes under test were attested. The smoke's are not.
    pub attested: bool,
    pub note: String,
    pub status: Status,
    /// Why the run failed before it could judge, if it did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub gates: Vec<Gate>,
    pub runtime: Runtime,
    pub started: String,
    pub finished: Option<String>,
    /// How long the whole scenario took, farm start to stop, in seconds: the number
    /// that shows a creeping beta.
    pub duration_secs: Option<f64>,
    pub builds: Builds,
    pub processes: Vec<ProcRecord>,
}

/// The RFC 3339 time now (UTC); empty if the time cannot be written in that form.
#[must_use]
pub fn now() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default()
}

/// The note every smoke document carries.
pub const SMOKE_NOTE: &str =
    "smoke: runs on its own cargo binaries, not the attested release bytes; unattested";

impl Document {
    /// A new `RUNNING` document for `scenario`, its identity from the CI environment
    /// (`GITHUB_SHA`, `GITHUB_RUN_ID`, `GITHUB_RUN_ATTEMPT`; `local` outside CI).
    #[must_use]
    pub fn running(scenario: &str, env: impl Fn(&str) -> Option<String>) -> Self {
        let get = |key: &str| env(key).unwrap_or_else(|| "local".to_owned());
        let commit = get("GITHUB_SHA");
        Self {
            schema_version: SCHEMA_VERSION,
            stage: STAGE,
            scenario: scenario.to_owned(),
            harness_commit: env(HARNESS_COMMIT_VAR).unwrap_or_else(|| commit.clone()),
            commit,
            set_hash: None,
            run_id: get("GITHUB_RUN_ID"),
            attempt: get("GITHUB_RUN_ATTEMPT"),
            attested: false,
            note: SMOKE_NOTE.to_owned(),
            status: Status::Running,
            error: None,
            gates: Vec::new(),
            runtime: Runtime::default(),
            started: now(),
            finished: None,
            duration_secs: None,
            builds: Builds::default(),
            processes: Vec::new(),
        }
    }

    /// The document's text.
    ///
    /// # Panics
    /// Never: the document is plain data.
    #[must_use]
    pub fn text(&self) -> String {
        serde_json::to_string_pretty(self).expect("the document serializes") + "\n"
    }

    /// The gate table, as the job summary shows it (Markdown).
    #[must_use]
    pub fn summary(&self) -> String {
        let mut out = format!(
            "## Beta `{}`: {}\n\n{}\n\n| gate | measured | threshold | verdict |\n|---|---|---|---|\n",
            self.scenario,
            self.status.as_str(),
            self.note
        );
        for g in &self.gates {
            let verdict = match g.verdict {
                Verdict::Pass => "PASS",
                Verdict::Fail => "FAIL",
            };
            out.push_str(&format!(
                "| {} | {} | {} | {} |\n",
                cell(&g.name),
                cell(&g.measured.to_string()),
                cell(&g.threshold),
                verdict
            ));
        }
        if let Some(error) = &self.error {
            out.push_str(&format!("\nerror: {error}\n"));
        }
        out.push_str(&format!(
            "\ndriver: {}; container: {}; buck2: {}\n",
            self.runtime.driver, self.runtime.container, self.runtime.buck2_version
        ));
        if let Some(secs) = self.duration_secs {
            out.push_str(&format!("\nduration: {secs:.0} s\n"));
        }
        out
    }
}

/// `text` as a cell of a Markdown table: a `|` in it would end the cell.
fn cell(text: &str) -> String {
    text.replace('|', "\\|")
}

/// The document being written to `path`: [`Writer::write`] saves its current state; its
/// drop saves it as `FAILED` if it was never finished.
pub struct Writer {
    path: PathBuf,
    pub doc: Document,
    finished: bool,
}

impl Writer {
    /// Writes the new document as `RUNNING` to `path`.
    ///
    /// # Panics
    /// When the file cannot be written: a run that cannot report must not run.
    #[must_use]
    pub fn start(path: &Path, doc: Document) -> Self {
        let result = Self {
            path: path.to_owned(),
            doc,
            finished: false,
        };
        result.write();
        result
    }

    /// Saves the document as it is now (write, then rename, so a reader never reads half).
    ///
    /// # Panics
    /// When the file cannot be written.
    pub fn write(&self) {
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, self.doc.text()).expect("the result file can be written");
        std::fs::rename(&tmp, &self.path).expect("the result file can be moved into place");
    }

    /// Ends the run with `status`, stamps the finish and saves.
    pub fn finish(&mut self, status: Status) {
        self.doc.status = status;
        self.doc.finished = Some(now());
        self.finished = true;
        self.write();
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        if !self.finished {
            if self.doc.error.is_none() {
                self.doc.error = Some("the harness ended before the scenario finished".to_owned());
            }
            self.doc.status = Status::Failed;
            self.doc.finished = Some(now());
            let _ = std::fs::write(&self.path, self.doc.text());
        }
    }
}

#[cfg(test)]
#[path = "result_tests.rs"]
mod tests;
