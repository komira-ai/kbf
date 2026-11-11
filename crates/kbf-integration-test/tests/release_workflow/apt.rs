//! The jobs of `release.yml` that publish the release's native .debs to the apt repository
//! (packages.komira.ai/apt): what each must keep so that no job holds both the signing key and
//! the cloud role, no such job compiles code, and nothing is written that was not attested.
//!
//! The jobs are `deploy-tool`, the three jobs of each suite and attempt ([`specs`]), and the two
//! install checks ([`CHECKS`]); their conditions come from `workflows::release_ifs`, which the
//! `workflows` lint reads too (the one lint decides which jobs may hold `id-token: write`).
//!
//! - **The graph.** Each job has exactly the `needs`, scopes, `if:` and environment of its row in
//!   [`rules`]: reads and writes hold `contents: read` and `id-token: write` in the environment
//!   `apt-publish` (the role, no key); indexes hold `contents: read` in `apt-signing` (the key,
//!   no role); `deploy-tool` and the checks hold `contents: read` alone and no environment.
//! - **No checkout, no build in a job that holds a secret or a token.** A read, index or write job
//!   has no checkout and runs no `cargo` or `rustup`: it downloads the `deploy-tool` artifact,
//!   which the token-less `deploy-tool` job built, and runs it.
//! - **The steps.** Each read, index and write job is exactly the steps of [`expected`], in that
//!   order: the download of the tool and `chmod`, the gate (`tool/xtask channels gate`, called
//!   with the settings, the secrets and the earlier jobs it needs), and then, each under exactly [`GATE_IF`], the
//!   rest. So a job without its settings ends green and does nothing, and a file the job uses was
//!   checked first: before an index, promotion or write that takes files from `release-assets`
//!   the job checks `SHA256SUMS` and each native .deb against its bundle (signer workflow
//!   `artifacts.yml`, `refs/heads/main`, this commit, and `--deny-self-hosted-runners`: the
//!   workflow lint reads that one flag as nothing, and refuses the word anywhere else).
//! - **The keys.** `APT_GPG_*` and every `secrets.` appear only in the index jobs, and there only
//!   in the gate and the sign step; no index job names the role or assumes it.
//! - **The version.** `verify` outputs `package-version` from the step that reads it from the two
//!   .debs, after the attestations are verified and before the files are uploaded; the jobs that
//!   use it take it from `needs.verify.outputs` into `PACKAGE_VERSION`, and nothing else.
//! - **The checks.** `check-apt-amd64` runs on `ubuntu-24.04`, `check-apt-arm64` on
//!   `ubuntu-24.04-arm`, each a literal label (the workflow lint refuses an expression); each
//!   checks out the commit, builds xtask and runs it in an Ubuntu container pinned by digest.

use super::{COMMIT_EXPR, Rule, command, output_is, uses, workflows};
use workflows::release_ifs::{self as ifs, ATTEMPTS, CHECKS, Kind, SUITES};

/// The job that builds the tool every other job here runs.
pub const DEPLOY_TOOL: &str = "deploy-tool";

/// The artifact the tool travels as.
const TOOL_ARTIFACT: &str = "deploy-tool";

/// The condition of every step after the gate: the job has its settings.
pub const GATE_IF: &str = "${{ steps.gate.outputs.enabled == 'true' }}";

/// The image the checks run in, pinned by digest.
const UBUNTU: &str =
    "ubuntu@sha256:fe81560990fae86dc4743202b2cb4d3df98d0bcf949173cecef15ce1127ef852";

/// Where the version of the native package comes from.
const VERSION_ENV: (&str, &str) = (
    "PACKAGE_VERSION",
    "${{ needs.verify.outputs.package-version }}",
);

/// The step of `verify` that reads the version from the two .debs.
const VERSION_STEP: &str =
    "cargo xtask channels apt version --debs dist --commit \"$COMMIT\" --output \"$GITHUB_OUTPUT\"";

/// A read, index or write job of one suite and attempt.
#[derive(Clone, Copy, Debug)]
pub struct Spec {
    pub suite: &'static str,
    pub kind: Kind,
    pub attempt: u8,
}

impl Spec {
    pub fn name(self) -> String {
        ifs::job_name(self.suite, self.kind, self.attempt)
    }

    fn of(self, kind: Kind) -> String {
        ifs::job_name(self.suite, kind, self.attempt)
    }

    /// The job of the same suite and kind in attempt `attempt`.
    fn write_before(self) -> String {
        ifs::job_name(self.suite, Kind::Write, self.attempt - 1)
    }

    fn suffix(self) -> String {
        if self.attempt == 1 {
            String::new()
        } else {
            format!("-{}", self.attempt)
        }
    }
}

/// Every read, index and write job, in file order.
pub fn specs() -> Vec<Spec> {
    let mut all = Vec::new();
    for suite in SUITES {
        for attempt in ATTEMPTS {
            for kind in Kind::ALL {
                all.push(Spec {
                    suite,
                    kind,
                    attempt,
                });
            }
        }
    }
    all
}

/// The read, index or write job called `job`.
pub fn spec(job: &str) -> Option<Spec> {
    specs().into_iter().find(|s| s.name() == job)
}

/// Whether `job` takes the tool as an artifact and compiles nothing (so it has no checkout).
pub fn consumes_tool(job: &str) -> bool {
    spec(job).is_some()
}

/// Whether `job` may hold `id-token: write` beside gamma: a read or a write.
pub fn may_hold_id_token(job: &str) -> bool {
    spec(job).is_some_and(|s| s.kind != Kind::Index)
}

/// The jobs of this module in file order: `deploy-tool`, `testing`, the checks, `stable`.
pub fn jobs() -> Vec<String> {
    let mut all = vec![DEPLOY_TOOL.to_owned()];
    for suite in SUITES {
        if suite == "stable" {
            all.extend(CHECKS.iter().map(|c| (*c).to_owned()));
        }
        all.extend(
            specs()
                .iter()
                .filter(|s| s.suite == suite)
                .map(|s| s.name()),
        );
    }
    all
}

/// The jobs that hold the cloud role or the signing key, for the rule that a write needs the
/// checks of the release.
pub fn holding() -> Vec<String> {
    specs().iter().map(|s| s.name()).collect()
}

fn strings(names: &[&str]) -> Vec<String> {
    names.iter().map(|n| (*n).to_owned()).collect()
}

/// The jobs `spec` needs.
fn needs(spec: Spec) -> Vec<String> {
    let mut all = strings(&["plan"]);
    let name = |kind| spec.of(kind);
    let checks: Vec<String> = CHECKS.iter().map(|c| (*c).to_owned()).collect();
    let verify = spec.kind == Kind::Index || (spec.kind == Kind::Write && spec.suite == "testing");
    if verify {
        all.push("verify".to_owned());
    }
    if spec.attempt == 1 && spec.suite == "stable" {
        all.extend(checks);
    }
    match (spec.kind, spec.attempt) {
        (Kind::Read, 1) if spec.suite == "testing" => all.push("publish".to_owned()),
        (Kind::Read, 1) => all.push("testing-apt-read".to_owned()),
        (Kind::Read, _) => all.push(spec.write_before()),
        (Kind::Index, 1) if spec.suite == "stable" => {
            all.push(name(Kind::Read));
            all.push("testing-apt-index".to_owned());
        }
        (Kind::Index, _) => all.push(name(Kind::Read)),
        (Kind::Write, _) => {
            all.push(name(Kind::Read));
            all.push(name(Kind::Index));
            if spec.attempt == 1 && spec.suite == "stable" {
                all.push("testing-apt-write".to_owned());
            }
        }
    }
    all.push(DEPLOY_TOOL.to_owned());
    all
}

/// The row of every job of this module for the graph rule.
pub fn rules() -> Vec<Rule> {
    let read = |scopes: &[(&str, &str)]| {
        scopes
            .iter()
            .map(|(s, l)| ((*s).to_owned(), (*l).to_owned()))
            .collect::<Vec<_>>()
    };
    let mut all = vec![Rule {
        job: DEPLOY_TOOL.to_owned(),
        needs: strings(&["plan"]),
        permissions: read(&[("contents", "read")]),
        condition: Some(ifs::plain()),
        environment: None,
    }];
    let checks = |all: &mut Vec<Rule>| {
        for check in CHECKS {
            let mut needs = strings(&["plan", "verify"]);
            needs.extend(ATTEMPTS.map(|n| ifs::job_name("testing", Kind::Write, n)));
            all.push(Rule {
                job: check.to_owned(),
                needs,
                permissions: read(&[("contents", "read")]),
                condition: Some(ifs::check_condition()),
                environment: None,
            });
        }
    };
    for suite in SUITES {
        if suite == "stable" {
            checks(&mut all);
        }
        for spec in specs().into_iter().filter(|s| s.suite == suite) {
            let (permissions, environment) = match spec.kind {
                Kind::Index => (read(&[("contents", "read")]), "apt-signing"),
                _ => (
                    read(&[("contents", "read"), ("id-token", "write")]),
                    "apt-publish",
                ),
            };
            all.push(Rule {
                job: spec.name(),
                needs: needs(spec),
                permissions,
                condition: Some(ifs::condition(spec.suite, spec.kind, spec.attempt)),
                environment: Some(environment.to_owned()),
            });
        }
    }
    all
}

/// What one step must be.
enum Want {
    Checkout,
    Download {
        name: String,
        path: String,
    },
    Upload {
        name: String,
        path: String,
    },
    Run {
        run: String,
        env: Vec<(String, String)>,
        dir: Option<&'static str>,
        id: Option<&'static str>,
    },
}

/// A step a job must have, why, and the condition it must be under.
struct Expect {
    why: String,
    want: Want,
    condition: Option<&'static str>,
}

fn run(why: &str, run: String) -> Expect {
    run_with(why, run, &[], None, None)
}

fn run_with(
    why: &str,
    run: String,
    env: &[(&str, &str)],
    dir: Option<&'static str>,
    id: Option<&'static str>,
) -> Expect {
    let mut env: Vec<(String, String)> = env
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    env.sort();
    Expect {
        why: why.to_owned(),
        want: Want::Run { run, env, dir, id },
        condition: None,
    }
}

fn under_gate(mut e: Expect) -> Expect {
    e.condition = Some(GATE_IF);
    e
}

fn download(why: &str, name: &str, path: &str) -> Expect {
    Expect {
        why: why.to_owned(),
        want: Want::Download {
            name: name.to_owned(),
            path: path.to_owned(),
        },
        condition: None,
    }
}

fn upload(why: &str, name: &str, path: &str) -> Expect {
    Expect {
        why: why.to_owned(),
        want: Want::Upload {
            name: name.to_owned(),
            path: path.to_owned(),
        },
        condition: None,
    }
}

const PUBLISH_SETTINGS: [&str; 4] = [
    "APT_PUBLISH_ROLE_ARN",
    "APT_BUCKET",
    "APT_DISTRIBUTION_ID",
    "APT_HOSTNAME",
];

/// The signing key's settings that are not secret material: the fingerprint is a variable.
const KEY_SETTINGS: [&str; 1] = ["APT_GPG_FINGERPRINT"];

/// The signing key's secret material, which the gate reads from the environment itself.
const KEY_SECRETS: [&str; 2] = ["APT_GPG_PRIVATE_KEY", "APT_GPG_PASSPHRASE"];

fn var(name: &str) -> String {
    format!("${{{{ vars.{name} }}}}")
}

fn secret(name: &str) -> String {
    format!("${{{{ secrets.{name} }}}}")
}

/// The gate's flags: a requirement and a setting carry their value from the step's `env`, expanded
/// by the shell inside one double-quoted argument; a secret is named only, and the verb reads it
/// from the environment.
fn flags(requires: &[&str], settings: &[&str], secrets: &[&str]) -> String {
    requires
        .iter()
        .map(|r| format!("--require \"{r}=${r}\""))
        .chain(settings.iter().map(|s| format!("--setting \"{s}=${s}\"")))
        .chain(secrets.iter().map(|s| format!("--secret {s}")))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The gate step of `spec`: the settings it needs and the earlier jobs it follows.
fn gate(spec: Spec) -> Expect {
    let name = spec.name();
    let mut env: Vec<(String, String)> = Vec::new();
    let mut requires: Vec<&str> = Vec::new();
    let mut settings: Vec<&str> = Vec::new();
    let mut secrets: Vec<&str> = Vec::new();
    let output = |job: String, key: &str| format!("${{{{ needs.{job}.outputs.{key} }}}}");
    match spec.kind {
        Kind::Read => {
            settings.extend(PUBLISH_SETTINGS);
            if spec.suite == "stable" && spec.attempt == 1 {
                requires.push("TESTING_ENABLED");
                env.push((
                    "TESTING_ENABLED".to_owned(),
                    output("testing-apt-read".to_owned(), "enabled"),
                ));
            }
        }
        Kind::Index => {
            requires.push("READ_ENABLED");
            env.push((
                "READ_ENABLED".to_owned(),
                output(spec.of(Kind::Read), "enabled"),
            ));
            settings.extend(KEY_SETTINGS);
            secrets.extend(KEY_SECRETS);
        }
        Kind::Write => {
            requires.extend(["READ_ENABLED", "INDEX_SIGNED"]);
            env.push((
                "READ_ENABLED".to_owned(),
                output(spec.of(Kind::Read), "enabled"),
            ));
            env.push((
                "INDEX_SIGNED".to_owned(),
                output(spec.of(Kind::Index), "signed"),
            ));
            settings.extend(PUBLISH_SETTINGS);
        }
    }
    for s in &settings {
        env.push(((*s).to_owned(), var(s)));
    }
    for s in &secrets {
        env.push(((*s).to_owned(), secret(s)));
    }
    env.sort();
    Expect {
        why: "the gate on the settings (`tool/xtask channels gate`, with the settings and the earlier jobs it needs), so that a job without them ends green and does nothing".to_owned(),
        want: Want::Run {
            run: format!(
                "tool/xtask channels gate --label {name} --output \"$GITHUB_OUTPUT\" {}",
                flags(&requires, &settings, &secrets)
            ),
            env,
            dir: None,
            id: Some("gate"),
        },
        condition: None,
    }
}

fn assume(spec: Spec) -> Expect {
    under_gate(run_with(
        "the assumption of the apt publish role through OIDC (`channels assume` with the token URL and a session of this job)",
        format!(
            "tool/xtask channels assume --role-arn \"$ROLE_ARN\" --session-name \"kbf-{}-$GITHUB_RUN_ID\" --region us-east-1 --env-file \"$GITHUB_ENV\" --token-url \"$ACTIONS_ID_TOKEN_REQUEST_URL\"",
            spec.name()
        ),
        &[("ROLE_ARN", "${{ vars.APT_PUBLISH_ROLE_ARN }}")],
        None,
        None,
    ))
}

/// The three steps that check what a job takes from `release-assets` before it uses it.
fn files_checked() -> Vec<Expect> {
    let mut all = vec![
        under_gate(download(
            "the download of `release-assets` into `dist`",
            "release-assets",
            "dist",
        )),
        under_gate(run_with(
            "the check of the files against SHA256SUMS",
            "sha256sum --check --strict SHA256SUMS".to_owned(),
            &[],
            Some("dist"),
            None,
        )),
    ];
    for (arch, platform) in [("amd64", "x86_64"), ("arm64", "arm64")] {
        let file = format!("dist/kbf-native-$COMMIT-linux-{platform}.deb");
        all.push(under_gate(run_with(
            &format!("the attestation check of the {arch} .deb, the one artifacts.yml attested from main at this commit (from its bundle)"),
            format!(
                "gh attestation verify \"{file}\" --bundle \"{file}.sigstore.json\" --repo \"$REPO\" --signer-workflow \"$REPO/.github/workflows/artifacts.yml\" --source-ref refs/heads/main --source-digest \"$COMMIT\" --deny-self-hosted-runners"
            ),
            &[
                ("GH_TOKEN", "${{ github.token }}"),
                ("REPO", "${{ github.repository }}"),
            ],
            None,
            None,
        )));
    }
    all
}

fn sign(spec: Spec) -> Expect {
    under_gate(run_with(
        "the signing of Release with the key, from the key's environment variables alone",
        format!(
            "tool/xtask channels apt sign --dir \"$RUNNER_TEMP/apt-next/dists/{}\" --work \"$RUNNER_TEMP/apt-sign\" --fingerprint \"$APT_GPG_FINGERPRINT\" --key-env APT_GPG_PRIVATE_KEY --passphrase-env APT_GPG_PASSPHRASE --output \"$GITHUB_OUTPUT\"",
            spec.suite
        ),
        &[
            ("APT_GPG_FINGERPRINT", &var("APT_GPG_FINGERPRINT")),
            ("APT_GPG_PRIVATE_KEY", &secret("APT_GPG_PRIVATE_KEY")),
            ("APT_GPG_PASSPHRASE", &secret("APT_GPG_PASSPHRASE")),
        ],
        None,
        Some("sign"),
    ))
}

const BUCKET: (&str, &str) = ("APT_BUCKET", "${{ vars.APT_BUCKET }}");

/// The steps a read, index or write job has after the gate.
fn after_gate(spec: Spec) -> Vec<Expect> {
    let s = spec.suffix();
    let owner = ("OWNER", "${{ github.repository_owner }}");
    let temp = "${{ runner.temp }}";
    match (spec.kind, spec.suite) {
        (Kind::Read, "testing") => vec![
            assume(spec),
            under_gate(run_with(
                "the read of the index of testing and the ETag of each object from S3",
                "tool/xtask channels apt read --suite testing --bucket \"$APT_BUCKET\" --out \"$RUNNER_TEMP/apt-current\"".to_owned(),
                &[BUCKET],
                None,
                None,
            )),
            under_gate(upload(
                "the upload of what was read",
                &format!("apt-current{s}"),
                &format!("{temp}/apt-current"),
            )),
        ],
        (Kind::Read, _) => vec![
            assume(spec),
            under_gate(run_with(
                "the read of the index of testing, what is promoted, from S3",
                "tool/xtask channels apt read --suite testing --bucket \"$APT_BUCKET\" --out \"$RUNNER_TEMP/apt-read/testing\"".to_owned(),
                &[BUCKET],
                None,
                None,
            )),
            under_gate(run_with(
                "the read of the index of stable, what it is merged into, from S3",
                "tool/xtask channels apt read --suite stable --bucket \"$APT_BUCKET\" --out \"$RUNNER_TEMP/apt-read/stable\"".to_owned(),
                &[BUCKET],
                None,
                None,
            )),
            under_gate(upload(
                "the upload of what was read",
                &format!("apt-read{s}"),
                &format!("{temp}/apt-read"),
            )),
        ],
        (Kind::Index, "testing") => {
            let mut all = vec![under_gate(download(
                "the download of what the read job read",
                &format!("apt-current{s}"),
                &format!("{temp}/apt-current"),
            ))];
            all.extend(files_checked());
            all.push(under_gate(run_with(
                "the merge of the .debs into the index that was read (`channels apt index` with the commit and `$PACKAGE_VERSION`)",
                "tool/xtask channels apt index --suite testing --current \"$RUNNER_TEMP/apt-current\" --out \"$RUNNER_TEMP/apt-next\" --debs dist --commit \"$COMMIT\" --version \"$PACKAGE_VERSION\" --origin \"$OWNER\" --label \"$OWNER\" --description \"$OWNER packages\"".to_owned(),
                &[owner],
                None,
                None,
            )));
            all.push(sign(spec));
            all.push(under_gate(upload(
                "the upload of the signed index",
                &format!("apt-next{s}"),
                &format!("{temp}/apt-next/dists\n{temp}/apt-next/etags.txt"),
            )));
            all
        }
        (Kind::Index, _) => {
            let mut all = vec![under_gate(download(
                "the download of what the read job read",
                &format!("apt-read{s}"),
                &format!("{temp}/apt-read"),
            ))];
            all.extend(files_checked());
            all.push(under_gate(run_with(
                "the promotion of this release from testing into the index of stable (`channels apt promote`, held to SHA256SUMS, with the commit and `$PACKAGE_VERSION`)",
                "tool/xtask channels apt promote --testing \"$RUNNER_TEMP/apt-read/testing\" --stable \"$RUNNER_TEMP/apt-read/stable\" --out \"$RUNNER_TEMP/apt-next\" --version \"$PACKAGE_VERSION\" --sums dist/SHA256SUMS --commit \"$COMMIT\" --origin \"$OWNER\" --label \"$OWNER\" --description \"$OWNER packages\"".to_owned(),
                &[owner],
                None,
                None,
            )));
            all.push(sign(spec));
            all.push(under_gate(upload(
                "the upload of the signed index",
                &format!("apt-promoted{s}"),
                &format!("{temp}/apt-next/dists\n{temp}/apt-next/etags.txt"),
            )));
            all
        }
        (Kind::Write, suite) => {
            let artifact = if suite == "testing" {
                format!("apt-next{s}")
            } else {
                format!("apt-promoted{s}")
            };
            let mut all = vec![under_gate(download(
                "the download of the signed index",
                &artifact,
                &format!("{temp}/apt-next"),
            ))];
            let pool = if suite == "testing" {
                all.extend(files_checked());
                "--debs dist --commit \"$COMMIT\" --version \"$PACKAGE_VERSION\""
            } else {
                "--no-pool"
            };
            all.push(assume(spec));
            let last = if spec.attempt == 3 { " --last" } else { "" };
            all.push(under_gate(run_with(
                "the write of the signed index with If-Match on the ETags that were read (the `.debs` once each first, for testing; `--last` on the third attempt alone)",
                format!(
                    "tool/xtask channels apt write --suite {suite} --bucket \"$APT_BUCKET\" --distribution-id \"$APT_DISTRIBUTION_ID\" --hostname \"$APT_HOSTNAME\" --next \"$RUNNER_TEMP/apt-next\" {pool}{last} --output \"$GITHUB_OUTPUT\""
                ),
                &[
                    BUCKET,
                    ("APT_DISTRIBUTION_ID", "${{ vars.APT_DISTRIBUTION_ID }}"),
                    ("APT_HOSTNAME", "${{ vars.APT_HOSTNAME }}"),
                ],
                None,
                Some("put"),
            )));
            all
        }
    }
}

/// Every step of a read, index or write job, in order.
fn expected(spec: Spec) -> Vec<Expect> {
    let mut all = vec![
        download(
            "the download of the `deploy-tool` artifact into `tool`",
            TOOL_ARTIFACT,
            "tool",
        ),
        run("`chmod +x tool/xtask`", "chmod +x tool/xtask".to_owned()),
        gate(spec),
    ];
    all.extend(after_gate(spec));
    all
}

/// The steps of a job that checks out the commit and builds with `cargo` (`deploy-tool` and the
/// checks), then `rest`.
fn built(build: &str, rest: Vec<Expect>) -> Vec<Expect> {
    let mut all = vec![
        Expect {
            why: "the checkout of the commit the pipeline run built".to_owned(),
            want: Want::Checkout,
            condition: None,
        },
        run(
            "`rustup toolchain install`",
            "rustup toolchain install".to_owned(),
        ),
        run(&format!("`{build}`"), build.to_owned()),
    ];
    all.extend(rest);
    all
}

fn keys_of(env: &[(String, String)]) -> Vec<(String, String)> {
    let mut env = env.to_vec();
    env.sort();
    env
}

fn is(step: &workflows::Step, want: &Want) -> bool {
    let with = || {
        let mut with = step.with.clone();
        with.sort();
        with
    };
    match want {
        Want::Checkout => {
            uses(step, "actions/checkout@")
                && with()
                    == [
                        ("persist-credentials".to_owned(), "false".to_owned()),
                        ("ref".to_owned(), COMMIT_EXPR.to_owned()),
                    ]
        }
        Want::Download { name, path } => {
            uses(step, "actions/download-artifact@")
                && with()
                    == [
                        ("name".to_owned(), name.clone()),
                        ("path".to_owned(), path.clone()),
                    ]
        }
        Want::Upload { name, path } => {
            // A block scalar's `path` ends with a line break the expectation does not have.
            let with: Vec<(String, String)> = with()
                .into_iter()
                .map(|(k, v)| (k, v.trim_end().to_owned()))
                .collect();
            uses(step, "actions/upload-artifact@")
                && with
                    == [
                        ("if-no-files-found".to_owned(), "error".to_owned()),
                        ("name".to_owned(), name.clone()),
                        ("path".to_owned(), path.clone()),
                        ("retention-days".to_owned(), "1".to_owned()),
                    ]
        }
        Want::Run { run, env, dir, id } => {
            step.uses.is_none()
                && step.run.as_deref().map(str::trim) == Some(run)
                && keys_of(&step.env) == *env
                && step.working_directory.as_deref() == *dir
                && step.id.as_deref() == *id
        }
    }
}

/// `steps` of `job` must be exactly `want`, in order, each under its condition: a problem for
/// each expected step that is missing, out of order or under another condition, and for each step
/// that is none of them.
fn same_steps(job: &str, steps: &[workflows::Step], want: &[Expect], out: &mut Vec<String>) {
    let mut taken = vec![false; steps.len()];
    let mut cursor = 0;
    for e in want {
        let found = (cursor..steps.len())
            .find(|&i| !taken[i] && is(&steps[i], &e.want))
            .or_else(|| (0..cursor).find(|&i| !taken[i] && is(&steps[i], &e.want)));
        let Some(at) = found else {
            out.push(format!("the job `{job}` has no step that is {}", e.why));
            continue;
        };
        if at < cursor {
            out.push(format!(
                "the step of `{job}` that is {} is out of order",
                e.why
            ));
        }
        cursor = cursor.max(at + 1);
        taken[at] = true;
        if steps[at].condition.as_deref() != e.condition
            || (steps[at].conditional && e.condition.is_none())
        {
            out.push(format!(
                "the step of `{job}` that is {} is not under {}",
                e.why,
                e.condition
                    .map_or("no condition".to_owned(), |c| format!("exactly `if: {c}`"))
            ));
        }
    }
    for step in steps
        .iter()
        .zip(&taken)
        .filter(|(_, t)| !**t)
        .map(|(s, _)| s)
    {
        out.push(format!(
            "the job `{job}` has a step that is none of its steps: {:?}",
            step.name
                .as_deref()
                .or(step.uses.as_deref())
                .or(step.run.as_deref())
                .unwrap_or("?")
        ));
    }
}

/// What a read, index or write job must not do: hold the repository's code.
fn no_build(job: &str, steps: &[workflows::Step], out: &mut Vec<String>) {
    for step in steps {
        if uses(step, "actions/checkout@") {
            out.push(format!(
                "the job `{job}` checks out the repository: a job that holds the cloud role or the signing key runs the `deploy-tool` artifact and compiles nothing"
            ));
        }
        let run = step.run.as_deref().unwrap_or_default();
        if run.contains("cargo") || run.contains("rustup") {
            out.push(format!(
                "the job `{job}` builds with `cargo`: a build script would run beside the cloud role or the signing key"
            ));
        }
    }
}

/// The rules of every job of this module.
pub fn problems(release: &str, out: &mut Vec<String>) -> Result<(), String> {
    for spec in specs() {
        let job = spec.name();
        let steps = workflows::job_steps(release, &job)?;
        no_build(&job, &steps, out);
        same_steps(&job, &steps, &expected(spec), out);
        let version =
            spec.kind == Kind::Index || (spec.kind == Kind::Write && spec.suite == "testing");
        let want_env = if version {
            vec![(VERSION_ENV.0.to_owned(), VERSION_ENV.1.to_owned())]
        } else {
            Vec::new()
        };
        job_env(release, &job, &want_env, out)?;
        runs_on(release, &job, "ubuntu-24.04", out)?;
        match spec.kind {
            Kind::Read => output_is(release, &job, "enabled", "gate", "enabled", out)?,
            Kind::Index => {
                output_is(release, &job, "enabled", "gate", "enabled", out)?;
                output_is(release, &job, "signed", "sign", "signed", out)?;
            }
            Kind::Write => output_is(release, &job, "conflict", "put", "conflict", out)?,
        }
    }
    deploy_tool(release, out)?;
    checks(release, out)?;
    keys(release, out)
}

fn job_env(
    release: &str,
    job: &str,
    want: &[(String, String)],
    out: &mut Vec<String>,
) -> Result<(), String> {
    let have = workflows::job_env(release, job)?;
    if have != want {
        out.push(format!(
            "the job `{job}` has the environment {have:?}, not exactly {want:?}: the version of the package is `needs.verify.outputs.package-version` in `PACKAGE_VERSION`, in the jobs that use it and nowhere else"
        ));
    }
    Ok(())
}

fn runs_on(release: &str, job: &str, want: &str, out: &mut Vec<String>) -> Result<(), String> {
    let have = workflows::job_runs_on(release, job)?;
    if have != want {
        out.push(format!(
            "the job `{job}` runs on `{have}`, not the literal label `{want}`: the workflow lint refuses an expression in `runs-on`, and each architecture is a job of its own"
        ));
    }
    Ok(())
}

fn deploy_tool(release: &str, out: &mut Vec<String>) -> Result<(), String> {
    let steps = workflows::job_steps(release, DEPLOY_TOOL)?;
    let want = built(
        "cargo build -p xtask --locked --release",
        vec![upload_tool()],
    );
    same_steps(DEPLOY_TOOL, &steps, &want, out);
    runs_on(release, DEPLOY_TOOL, "ubuntu-24.04", out)?;
    job_env(release, DEPLOY_TOOL, &[], out)
}

fn upload_tool() -> Expect {
    upload(
        "the upload of `target/release/xtask` as `deploy-tool`",
        TOOL_ARTIFACT,
        "target/release/xtask",
    )
}

fn checks(release: &str, out: &mut Vec<String>) -> Result<(), String> {
    for (job, arch, runner) in [
        (CHECKS[0], "amd64", "ubuntu-24.04"),
        (CHECKS[1], "arm64", "ubuntu-24.04-arm"),
    ] {
        let steps = workflows::job_steps(release, job)?;
        let docker = run(
            "the install check in a clean container, `docker run` of the xtask built here in an Ubuntu pinned by digest",
            format!(
                "docker run --rm --volume \"$PWD/target/debug/xtask:/usr/local/bin/xtask:ro\" {UBUNTU} xtask channels check-apt --arch {arch} --version \"$PACKAGE_VERSION\" --commit \"$COMMIT\""
            ),
        );
        same_steps(
            job,
            &steps,
            &built("cargo build -p xtask --locked", vec![docker]),
            out,
        );
        runs_on(release, job, runner, out)?;
        job_env(
            release,
            job,
            &[(VERSION_ENV.0.to_owned(), VERSION_ENV.1.to_owned())],
            out,
        )?;
    }
    Ok(())
}

/// The key and the role are never in one job, and no job but the index jobs names the key.
fn keys(release: &str, out: &mut Vec<String>) -> Result<(), String> {
    let all = super::jobs();
    for job in &all {
        let text = super::job_text(release, job);
        let found = spec(job);
        let index = found.is_some_and(|s| s.kind == Kind::Index);
        if text.contains("APT_GPG_") && !index {
            out.push(format!(
                "the job `{job}` reads `APT_GPG_*`: only the apt index jobs hold the signing key"
            ));
        }
        if found.is_none() && text.contains("APT_PUBLISH_ROLE_ARN") {
            out.push(format!(
                "the job `{job}` reads `APT_PUBLISH_ROLE_ARN`: only the apt read and write jobs hold the role"
            ));
        }
        if !index {
            continue;
        }
        if text.contains("APT_PUBLISH_ROLE_ARN") || text.contains("channels assume") {
            out.push(format!(
                "the job `{job}` holds the signing key and the cloud role: no job holds both"
            ));
        }
        for step in workflows::job_steps(release, job)? {
            let holds = step
                .env
                .iter()
                .chain(&step.with)
                .any(|(_, v)| v.contains("secrets.") || v.contains("APT_GPG_PRIVATE_KEY"))
                || step.run.as_deref().is_some_and(|r| r.contains("secrets."));
            let allowed = matches!(step.id.as_deref(), Some("gate" | "sign"));
            if holds && !allowed {
                out.push(format!(
                    "a step of `{job}` ({:?}) holds the signing key: only the gate and the sign step may",
                    step.name.as_deref().unwrap_or("?")
                ));
            }
        }
    }
    Ok(())
}

/// `verify` outputs the version of the native package from the step that reads it from the two
/// .debs, after the attestations and before the upload.
pub fn package_version(release: &str, out: &mut Vec<String>) -> Result<(), String> {
    let job = "verify";
    output_is(
        release,
        job,
        "package-version",
        "package",
        "package-version",
        out,
    )?;
    let steps = workflows::job_steps(release, job)?;
    let at = steps.iter().position(|s| {
        s.id.as_deref() == Some("package")
            && s.run.as_deref().map(str::trim) == Some(VERSION_STEP)
            && s.condition.is_none()
            && !s.conditional
    });
    let attested = command(&steps, "cargo xtask verify-assets attestations ").map(|(at, ..)| at);
    let uploaded = steps
        .iter()
        .position(|s| uses(s, "actions/upload-artifact@"));
    match at {
        None => out.push(format!(
            "the job `{job}` has no unconditional step with `id: package` that runs `{VERSION_STEP}`"
        )),
        Some(at) => {
            if attested.is_none_or(|a| a >= at) || uploaded.is_none_or(|u| u <= at) {
                out.push(format!(
                    "the step of `{job}` that reads the package version is not after the attestations and before the upload of `release-assets`: the version would come from files no one verified"
                ));
            }
        }
    }
    Ok(())
}
