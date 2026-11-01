//! Tests of the seed-build lint. The real workflows are the control (no problem); each other
//! test spoils one copy of them and names the rule it catches. A spoil must change the text,
//! and it is made inside the one job it names, so a rule whose anchor text moved, or that
//! only read another job, cannot go quiet.

use super::*;

mod builds;
mod published;

const ARTIFACTS: &str = ".github/workflows/artifacts.yml";
const PIPELINE: &str = ".github/workflows/pipeline.yml";
const RELEASE: &str = ".github/workflows/release.yml";

fn read(path: &str) -> String {
    std::fs::read_to_string(crate::repo_root().join(path)).expect(path)
}

/// The real workflows, as owned texts a test may spoil.
struct Real {
    artifacts: String,
    pipeline: String,
    release: String,
    called: Vec<(String, String)>,
}

fn real() -> Real {
    let pipeline = read(PIPELINE);
    Real {
        artifacts: read(ARTIFACTS),
        called: called(&crate::repo_root(), &pipeline),
        pipeline,
        release: read(RELEASE),
    }
}

impl Real {
    fn problems(&self) -> Vec<String> {
        problems(&Workflows {
            artifacts: &self.artifacts,
            pipeline: &self.pipeline,
            release: &self.release,
            called: self.called.clone(),
        })
        .unwrap_or_else(|e| vec![e])
    }
}

/// The lines of the job `job` of `text`: its header line to the next job's, comments included.
fn job_span(text: &str, job: &str) -> (Vec<String>, usize, usize) {
    let header = format!("  {job}:");
    let all: Vec<String> = text.split_inclusive('\n').map(str::to_owned).collect();
    let start = all
        .iter()
        .position(|l| l.trim_end() == header)
        .unwrap_or_else(|| panic!("no job `{job}`"));
    let end = all[start + 1..]
        .iter()
        .position(|l| l.starts_with("  ") && !l.starts_with("   ") && l.trim_end().ends_with(':'))
        .map_or(all.len(), |n| start + 1 + n);
    (all, start, end)
}

/// `text` with `from` replaced by `to` inside the job `job` only, which must hold `from`
/// exactly once.
fn in_job(text: &str, job: &str, from: &str, to: &str) -> String {
    let (all, start, end) = job_span(text, job);
    let region = all[start..end].concat();
    assert_eq!(
        region.matches(from).count(),
        1,
        "job `{job}` holds `{from}` {} times",
        region.matches(from).count()
    );
    [
        all[..start].concat(),
        region.replace(from, to),
        all[end..].concat(),
    ]
    .concat()
}

/// `text` without the job `job`.
fn without_job(text: &str, job: &str) -> String {
    let (all, start, end) = job_span(text, job);
    [all[..start].concat(), all[end..].concat()].concat()
}

fn red(found: &[String], want: &str) {
    assert!(
        found.iter().any(|p| p.contains(want)),
        "want a problem with `{want}`: {found:#?}"
    );
}

/// The problems of the real workflows with `from` replaced by `to` in the job `job` of
/// artifacts.yml.
fn spoil_artifacts(job: &str, from: &str, to: &str) -> Vec<String> {
    let mut w = real();
    w.artifacts = in_job(&w.artifacts, job, from, to);
    w.problems()
}

/// The `cargo build` of `kbf` in each seed job, as the real artifacts.yml writes it.
const KBF_BUILDS: &[(&str, &str)] = &[
    (
        "darwin-arm64",
        "- run: cargo build --locked --release --target \"$TARGET\" -p kbf-cli --bin kbf\n",
    ),
    (
        "linux-x86_64",
        "- run: cargo build --locked --release --target \"$TARGET\" -p kbf-node -p kbf-server -p kbf-cli --bins\n",
    ),
    (
        "linux-arm64",
        "- run: cargo build --locked --release --target \"$TARGET\" -p kbf-node -p kbf-server -p kbf-cli --bins\n",
    ),
];

/// The `runs-on` line of each seed job.
const RUNNERS: &[(&str, &str)] = &[
    ("darwin-arm64", "    runs-on: macos-15\n"),
    ("linux-x86_64", "    runs-on: ubuntu-24.04\n"),
    ("linux-arm64", "    runs-on: ubuntu-24.04-arm\n"),
];

/// The problems of the real workflows with `to` in place of each seed job's kbf build, `{build}`
/// in `to` standing for that build's step.
fn around_kbf_build(job: &str, build: &str, to: &str) -> Vec<String> {
    spoil_artifacts(job, build, &to.replace("{build}", build))
}

#[test]
fn the_real_workflows_meet_every_rule() {
    // Catches: the control for every spoil below failing on the real workflows.
    let found = real().problems();
    assert!(found.is_empty(), "{}", found.join("\n"));
}
