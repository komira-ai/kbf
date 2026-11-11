//! The frontend's deploy to Docker Swarm: the jobs `deploy-digest` and `deploy` of
//! `.github/workflows/release.yml`.
//!
//! - **When.** Both run under [`DEPLOY_IF`] alone: the release condition of every writing job
//!   and the repository variables `CP_DEPLOY_ENDPOINT` and `CP_DEPLOY_TAILSCALE_TAGS` not
//!   empty, so a repository that sets no endpoint (a fork, or before the deploy front exists)
//!   or no tailnet tag skips both and the release ends as it did without them; the call never
//!   asks for an empty tag. `deploy-digest` needs `plan` and `publish`; `deploy` needs `plan`,
//!   `gamma` (the frontend takes a release only after the farm has) and `deploy-digest`.
//! - **`deploy-digest`** holds `contents: read` alone and runs exactly [`DIGEST_RUNS`]: the
//!   toolchain, the build of xtask, and `release image-digest` for this repository, the planned
//!   tag and `COMMIT`, into `GITHUB_OUTPUT`, with the run's token to download the release and
//!   nothing else in its `env:`. It uses no action but its checkout, so it reads no artifact
//!   and joins no network. Its outputs are the verb's three lines, by name.
//! - **`deploy`** calls exactly [`CALLED`] (docker-swarm-updater's deploy workflow at a full
//!   commit; the workflow lint holds the version comment) with exactly the inputs [`WITH`]:
//!   the service `kbf-frontend`, the released commit, the digest job's three outputs, the
//!   endpoint, audience, Tailscale client and tailnet tags of the `CP_DEPLOY_*` repository
//!   variables, the environment `controlplane` and a wait of 240 minutes.
//!   It holds `id-token: write` alone (the graph's rule) and passes no secret, no matrix and no
//!   `env:`: the called workflow asks GitHub for an OIDC token per request, which is all it is
//!   given.
//! - **Who reads `CP_DEPLOY_*`.** Only these two jobs: the endpoint and the tailnet tags in
//!   their `if:`, the rest in `deploy`'s inputs.

use super::{GAMMA, PUBLISH, Rule, env_is, flag_is, job_text, lines, output_is, run_of, workflows};

/// The job that reads the image's digest and addresses from the published release.
pub const DIGEST: &str = "deploy-digest";
/// The job that calls docker-swarm-updater's deploy workflow.
pub const DEPLOY: &str = "deploy";

/// The `if:` of both jobs: [`super::RELEASE_IF`], the deploy front's address set and the
/// tailnet tags set.
pub const DEPLOY_IF: &str = "${{ github.event.workflow_run.conclusion == 'success' && github.event.workflow_run.event == 'push' && github.event.workflow_run.head_branch == 'main' && github.event.workflow_run.head_repository.full_name == github.repository && needs.plan.outputs.code == 'true' && needs.plan.outputs.pipeline != 'true' && vars.CP_DEPLOY_ENDPOINT != '' && vars.CP_DEPLOY_TAILSCALE_TAGS != '' }}";

/// The reusable workflow `deploy` calls: docker-swarm-updater's deploy workflow at v0.1.3.
pub const CALLED: &str = "komira-ai/docker-swarm-updater/.github/workflows/deploy.yml@c1fcc47610e2886d4d9e561be3c0c76f0452d1f6";

/// The inputs `deploy` passes, all of them.
pub const WITH: [(&str, &str); 12] = [
    ("service", "kbf-frontend"),
    ("version", "${{ github.event.workflow_run.head_sha }}"),
    ("image-digest", "${{ needs.deploy-digest.outputs.digest }}"),
    ("image-url", "${{ needs.deploy-digest.outputs.image-url }}"),
    (
        "bundle-url",
        "${{ needs.deploy-digest.outputs.bundle-url }}",
    ),
    ("endpoint", "${{ vars.CP_DEPLOY_ENDPOINT }}"),
    ("audience", "${{ vars.CP_DEPLOY_AUDIENCE }}"),
    ("environment", "controlplane"),
    ("wait-minutes", "240"),
    (
        "tailscale-oauth-client-id",
        "${{ vars.CP_DEPLOY_TAILSCALE_CLIENT_ID }}",
    ),
    (
        "tailscale-audience",
        "${{ vars.CP_DEPLOY_TAILSCALE_AUDIENCE }}",
    ),
    ("tailscale-tags", "${{ vars.CP_DEPLOY_TAILSCALE_TAGS }}"),
];

/// The verb of `deploy-digest`.
const VERB: &str = "cargo xtask release image-digest ";

/// Every `run:` of `deploy-digest`, in order.
const DIGEST_RUNS: [&str; 3] = [
    "rustup toolchain install",
    "cargo build -p xtask --locked",
    "cargo xtask release image-digest --repo \"$GITHUB_REPOSITORY\" --tag \"$TAG\" --commit \"$COMMIT\" --output \"$GITHUB_OUTPUT\"",
];

/// The job keys `deploy` may not have: it has no runner or steps of its own, and is given
/// no secret, matrix or environment variable.
const CALL_KEYS_REFUSED: [&str; 5] = ["secrets", "strategy", "env", "runs-on", "steps"];

/// The rules of the two jobs in the graph.
pub fn rules() -> Vec<Rule> {
    vec![
        rule(DIGEST, &["plan", PUBLISH], &[("contents", "read")]),
        rule(DEPLOY, &["plan", GAMMA, DIGEST], &[("id-token", "write")]),
    ]
}

/// One rule: both jobs run under [`DEPLOY_IF`] and in no environment.
fn rule(job: &str, needs: &[&str], permissions: &[(&str, &str)]) -> Rule {
    Rule {
        job: job.to_owned(),
        needs: needs.iter().map(|n| (*n).to_owned()).collect(),
        permissions: permissions
            .iter()
            .map(|(s, l)| ((*s).to_owned(), (*l).to_owned()))
            .collect(),
        condition: Some(DEPLOY_IF.to_owned()),
        environment: None,
    }
}

/// Whether `job` calls a reusable workflow (and so has no steps of its own).
pub fn calls(release: &str, job: &str) -> Result<bool, String> {
    Ok(workflows::job_uses(release, job)?.is_some())
}

/// What differs between the two jobs and the rules above.
pub fn problems(release: &str, out: &mut Vec<String>) -> Result<(), String> {
    digest_job(release, out)?;
    deploy_job(release, out)?;
    for job in workflows::job_names(release)? {
        if job != DIGEST && job != DEPLOY && job_text(release, &job).contains("CP_DEPLOY_") {
            out.push(format!(
                "the job `{job}` reads a `CP_DEPLOY_*` variable: only `{DIGEST}` and `{DEPLOY}` may"
            ));
        }
    }
    Ok(())
}

fn digest_job(release: &str, out: &mut Vec<String>) -> Result<(), String> {
    let steps = workflows::job_steps(release, DIGEST)?;
    let runs: Vec<String> = steps
        .iter()
        .filter(|s| s.run.is_some())
        .flat_map(|s| lines(s).map(str::to_owned).collect::<Vec<_>>())
        .collect();
    if runs != DIGEST_RUNS {
        out.push(format!(
            "the job `{DIGEST}` runs {runs:?}, not exactly {DIGEST_RUNS:?}"
        ));
    }
    for step in &steps {
        let Some(action) = step.uses.as_deref() else {
            continue;
        };
        if !action.starts_with("actions/checkout@") {
            out.push(format!(
                "the job `{DIGEST}` uses `{action}`: it reads the published release and nothing else"
            ));
        }
    }
    let verb = VERB.trim_end();
    if let Some((_, step, words)) = run_of(DIGEST, &steps, VERB, out) {
        flag_is(&words, verb, "--repo", "$GITHUB_REPOSITORY", out);
        flag_is(&words, verb, "--tag", "$TAG", out);
        flag_is(&words, verb, "--commit", "$COMMIT", out);
        flag_is(&words, verb, "--output", "$GITHUB_OUTPUT", out);
        env_is(DIGEST, step, "TAG", "${{ needs.plan.outputs.tag }}", out);
        env_is(DIGEST, step, "GH_TOKEN", "${{ github.token }}", out);
        if step.env.len() != 2 {
            out.push(format!(
                "the step of `{DIGEST}` that runs `{verb}` sets {:?}, not exactly `TAG` and `GH_TOKEN`",
                step.env.iter().map(|(k, _)| k).collect::<Vec<_>>()
            ));
        }
        let id = step.id.as_deref().unwrap_or("");
        for name in ["digest", "image-url", "bundle-url"] {
            output_is(release, DIGEST, name, id, name, out)?;
        }
    }
    let outputs = workflows::job_outputs(release, DIGEST)?;
    if outputs.len() != 3 {
        out.push(format!(
            "the job `{DIGEST}` outputs {:?}, not exactly `digest`, `image-url` and `bundle-url`",
            outputs.iter().map(|(k, _)| k).collect::<Vec<_>>()
        ));
    }
    Ok(())
}

fn deploy_job(release: &str, out: &mut Vec<String>) -> Result<(), String> {
    let called = workflows::job_uses(release, DEPLOY)?;
    if called.as_deref() != Some(CALLED) {
        out.push(format!(
            "the job `{DEPLOY}` calls {called:?}, not exactly `{CALLED}`"
        ));
    }
    let mut with = workflows::job_with(release, DEPLOY)?;
    with.sort();
    let mut want: Vec<(String, String)> = WITH
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    want.sort();
    if with != want {
        out.push(format!(
            "the job `{DEPLOY}` passes {with:?}, not exactly {want:?}"
        ));
    }
    for key in CALL_KEYS_REFUSED {
        if workflows::job_has_key(release, DEPLOY, key)? {
            out.push(format!(
                "the job `{DEPLOY}` has `{key}:`: it calls the deploy workflow with its inputs and the OIDC token alone"
            ));
        }
    }
    Ok(())
}
