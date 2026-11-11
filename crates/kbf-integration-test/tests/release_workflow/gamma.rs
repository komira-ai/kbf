//! The gamma stage of `release.yml`: what the job must keep now that hostkeeper deploys the
//! farm's Linux nodes (`hostkeeper-desired.yml`, after this workflow, when the repository variable
//! `KBF_SETS_DESIRED` is `true`) and `gamma` no longer sets kbf's update coordinator.
//!
//! - **Retired.** No job reads a variable of the retired deploy step ([`RETIRED`]) and `gamma`
//!   does not run `cargo xtask deploy-farm` or `release farm-version`, so the job cannot wait on
//!   an update coordinator whose role table is empty.
//! - **The notice.** The first step after the checkout prints a `::notice::` ([`NOTICE`]) with no
//!   expression in it.
//! - **The switch.** Every later step is under exactly [`SETS_IF`] (the tailnet join under
//!   [`JOIN_IF`], which adds the two variables it needs), so with `KBF_SETS_DESIRED` unset the job
//!   ends green after the notice and holds no token's use.
//! - **The tailnet join.** At most one step of [`TS_ACTION`], after the build of xtask and before
//!   the proof, given exactly [`TS_WITH`] (variables and the tag; the run token, no secret). No
//!   other job uses that action.
//! - **The proof.** The pinned Buck2 is fetched (the fetch line of [`BUCK2_FETCH`], the one place
//!   the job may name an `https://` address) and `cargo xtask prove-farm` runs once, given the
//!   four organisation variables ([`FARM_VARS`]) through the step's `env:` and nothing else of
//!   the environment but the run id and attempt as the nonce ([`NONCE`]); no other job reads a
//!   `FARM_` variable, and no address is written.
//! - **Time.** Every step has a `timeout-minutes` of its own and the job's is at least their
//!   sum, so a step that hangs is cut by its own timeout.

use super::{GAMMA, code, env_is, flag_is, job_text, lines, run_of, uses, workflows};

/// The condition of every step of `gamma` after the notice: the repository variable that says
/// hostkeeper deploys the farm.
pub const SETS_IF: &str = "${{ vars.KBF_SETS_DESIRED == 'true' }}";

/// The condition of the network join: [`SETS_IF`], the trust credential's client id is set, and
/// so is the farm's address (the proof reaches the farm over that network).
pub const JOIN_IF: &str =
    "${{ vars.KBF_SETS_DESIRED == 'true' && vars.TS_CLIENT_ID != '' && vars.FARM_ADDRESS != '' }}";

/// What the first step after the checkout runs, up to the message.
pub const NOTICE: &str = "echo \"::notice title=gamma::";

/// What the retired deploy step read, which no job reads any more.
pub const RETIRED: [&str; 3] = [
    "KBF_COORDINATOR_",
    "KBF_MIN_UP_",
    "KBF_REQUIRE_COORDINATOR_CURRENT",
];

/// The verbs of the retired deploy and its version, which `gamma` does not run.
const RETIRED_VERBS: [&str; 2] = ["deploy-farm", "farm-version"];

/// The organisation variables of the farm the proof builds against, as the `env:` key of the
/// proof step that reads each and the variable.
pub const FARM_VARS: [(&str, &str); 4] = [
    ("FARM_ADDRESS", "${{ vars.FARM_ADDRESS }}"),
    ("FARM_INSTANCE", "${{ vars.FARM_INSTANCE }}"),
    ("FARM_TLS", "${{ vars.FARM_TLS }}"),
    (
        "FARM_PROPERTIES",
        "${{ vars.FARM_LINUX_X86_64_PROPERTIES }}",
    ),
];

/// The nonce of the proof: every action of the project takes it, so each run's are new.
pub const NONCE: &str = "${{ github.run_id }}-${{ github.run_attempt }}";

/// The step of `gamma` that fetches Buck2, exactly as `beta.yml` does.
pub const BUCK2_FETCH: &str = "cargo xtask fetch --url \"https://github.com/facebook/buck2/releases/download/$BUCK2_VERSION/buck2-x86_64-unknown-linux-gnu.zst\" --sha256 \"$BUCK2_SHA256\" --out \"$RUNNER_TEMP/tools/buck2\" --extract zst";

/// The action `gamma` joins a Tailscale network with, pinned by commit.
pub const TS_ACTION: &str = "tailscale/github-action@";

/// The inputs of the network join, all of them: workload identity federation with the run's
/// token (the CI trust credential, `TS_CLIENT_ID` and `TS_AUDIENCE`, as `tag:ci`), so no
/// `authkey` or `oauth-secret`.
pub const TS_WITH: [(&str, &str); 3] = [
    ("oauth-client-id", "${{ vars.TS_CLIENT_ID }}"),
    ("audience", "${{ vars.TS_AUDIENCE }}"),
    ("tags", "tag:ci"),
];

/// The name of a step, for a message.
fn name_of(step: &workflows::Step) -> &str {
    step.name
        .as_deref()
        .or(step.uses.as_deref())
        .or(step.run.as_deref())
        .unwrap_or("?")
}

/// The rules of the gamma stage.
pub fn rules(release: &str, out: &mut Vec<String>) -> Result<(), String> {
    let job = GAMMA;
    let steps = workflows::job_steps(release, job)?;
    let all = code(release);
    for word in RETIRED {
        if all.contains(word) {
            out.push(format!(
                "the workflow reads `{word}`: the farm's Linux nodes are deployed by hostkeeper, and `{job}` no longer sets kbf's update coordinator"
            ));
        }
    }
    let text = job_text(release, job);
    for verb in RETIRED_VERBS {
        if text.contains(verb) {
            out.push(format!(
                "the job `{job}` runs or names `{verb}`: it would wait on an update coordinator that no longer deploys the farm"
            ));
        }
    }
    for other in super::jobs().iter().filter(|j| j.as_str() != job) {
        if job_text(release, other).contains("vars.FARM_") {
            out.push(format!(
                "the job `{other}` reads a `FARM_` variable: the farm's address and platform are `{job}`'s alone"
            ));
        }
    }
    // The pinned Buck2 download is the one address the job names.
    let text: String = text
        .lines()
        .filter(|l| !l.contains(BUCK2_FETCH))
        .collect::<Vec<_>>()
        .join("\n");
    if text.contains("http://") || text.contains("https://") {
        out.push(format!(
            "the job `{job}` writes an address: nothing but the pinned Buck2 download is named"
        ));
    }
    if text.contains("grpc://") || text.contains("grpcs://") {
        out.push(format!(
            "the job `{job}` writes a farm address: it is the organisation variable `FARM_ADDRESS`, never a literal"
        ));
    }
    if text.contains("secrets.") {
        out.push(format!(
            "the job `{job}` reads a secret: it holds the run's OIDC token and nothing else"
        ));
    }
    notice_and_switch(&steps, out);
    let prove = prove_farm(&steps, out);
    tailnet_join(release, &steps, prove, out)?;
    time(release, &steps, out)
}

/// The notice is the second step, unconditional; every later step is under [`SETS_IF`], the
/// network join under [`JOIN_IF`].
fn notice_and_switch(steps: &[workflows::Step], out: &mut Vec<String>) {
    let job = GAMMA;
    match steps.get(1) {
        Some(step)
            if step.condition.is_none()
                && !step.conditional
                && step.run.as_deref().is_some_and(|r| {
                    r.starts_with(NOTICE)
                        && r.ends_with('"')
                        && !r.contains("${{")
                        && !r.contains('\n')
                        && !r.contains('$')
                }) => {}
        _ => out.push(format!(
            "the second step of `{job}`, the first after the checkout, is not an unconditional one-line `{NOTICE}...\"` with no expression in it: the job must say that hostkeeper deploys the farm"
        )),
    }
    for step in steps.iter().skip(2) {
        let want = if uses(step, TS_ACTION) {
            JOIN_IF
        } else {
            SETS_IF
        };
        if step.condition.as_deref() != Some(want) {
            out.push(format!(
                "the step {:?} of `{job}` after the notice is not under exactly `if: {want}`: with `KBF_SETS_DESIRED` unset the job must end after the notice",
                name_of(step)
            ));
        }
    }
}

/// The proof step of `gamma`: Buck2 fetched as `beta.yml` does, then one `prove-farm` given the
/// four `FARM_` variables and the nonce through `env:` and the fetched Buck2 and the project by
/// their paths. The index of the proof step, when there is one.
fn prove_farm(steps: &[workflows::Step], out: &mut Vec<String>) -> Option<usize> {
    let job = GAMMA;
    let verb = "cargo xtask prove-farm";
    let (at, step, words) = run_of(job, steps, "cargo xtask prove-farm ", out)?;
    let count = steps
        .iter()
        .filter(|s| lines(s).any(|l| l.starts_with("cargo xtask prove-farm ")))
        .count();
    if count != 1 {
        out.push(format!("`{job}` runs `{verb}` {count} times, not once"));
    }
    let fetched = steps
        .iter()
        .position(|s| s.run.as_deref().map(str::trim) == Some(BUCK2_FETCH));
    if fetched.is_none_or(|f| f > at) {
        out.push(format!(
            "`{job}` does not fetch Buck2 with `{BUCK2_FETCH}` before `{verb}`"
        ));
    }
    if let Some(f) = fetched
        && !steps[f].env.is_empty()
    {
        out.push(format!(
            "the Buck2 fetch step of `{job}` has an `env:`: its pins are the job's `BUCK2_VERSION` and `BUCK2_SHA256`, which the pins lint ties to beta.yml, and a step's own could fetch other bytes"
        ));
    }
    flag_is(&words, verb, "--address", "$FARM_ADDRESS", out);
    flag_is(&words, verb, "--instance", "$FARM_INSTANCE", out);
    flag_is(&words, verb, "--tls", "$FARM_TLS", out);
    flag_is(&words, verb, "--properties", "$FARM_PROPERTIES", out);
    flag_is(&words, verb, "--buck2", "$RUNNER_TEMP/tools/buck2", out);
    flag_is(
        &words,
        verb,
        "--project",
        "crates/kbf-integration-test/gamma/buck2",
        out,
    );
    flag_is(&words, verb, "--nonce", "$NONCE", out);
    for (key, want) in FARM_VARS {
        env_is(job, step, key, want, out);
    }
    env_is(job, step, "NONCE", NONCE, out);
    if step.env.len() != FARM_VARS.len() + 1 {
        out.push(format!(
            "the step of `{job}` that runs `{verb}` has {} `env:` entries, not the four `FARM_` variables and the nonce",
            step.env.len()
        ));
    }
    Some(at)
}

/// The network join of `gamma`, if there is one: one step, after the build of xtask and before
/// the proof (at `prove_at`), given exactly [`TS_WITH`] and no `env:`; and in no other job. Its
/// condition is held with every other step's, by [`notice_and_switch`].
fn tailnet_join(
    release: &str,
    steps: &[workflows::Step],
    prove_at: Option<usize>,
    out: &mut Vec<String>,
) -> Result<(), String> {
    for job in super::jobs().iter().filter(|j| j.as_str() != GAMMA) {
        if super::deploy::calls(release, job)? {
            continue;
        }
        if workflows::job_steps(release, job)?
            .iter()
            .any(|s| uses(s, TS_ACTION))
        {
            out.push(format!(
                "the job `{job}` uses `{TS_ACTION}`: only `{GAMMA}` joins the farm's network"
            ));
        }
    }
    let joins: Vec<usize> = (0..steps.len())
        .filter(|&i| uses(&steps[i], TS_ACTION))
        .collect();
    if joins.len() > 1 {
        out.push(format!(
            "`{GAMMA}` uses `{TS_ACTION}` {} times, not once",
            joins.len()
        ));
    }
    let built = steps.iter().position(|s| {
        s.run
            .as_deref()
            .is_some_and(|r| r.trim() == "cargo build -p xtask --locked")
    });
    for &i in &joins {
        let step = &steps[i];
        let mut with: Vec<(&str, &str)> = step
            .with
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        with.sort_unstable();
        let mut want = TS_WITH.to_vec();
        want.sort_unstable();
        if with != want {
            out.push(format!(
                "the network join of `{GAMMA}` is given {with:?}, not exactly {want:?}: the client id and audience are the CI trust credential's variables, the tag is `tag:ci`, and the run token is its only credential"
            ));
        }
        if !step.env.is_empty() {
            out.push(format!(
                "the network join of `{GAMMA}` has an `env:`: its inputs are the three of `with:` alone"
            ));
        }
        if prove_at.is_some_and(|p| i > p) || built.is_none_or(|b| i < b) {
            out.push(format!(
                "the network join of `{GAMMA}` is not after `cargo build -p xtask` and before `cargo xtask prove-farm`: the build's scripts would see the network, or the proof would run without it"
            ));
        }
    }
    Ok(())
}

/// The minutes of a step's or the job's `timeout-minutes`, if it is a whole number.
fn minutes(text: Option<&str>) -> Option<u64> {
    text.and_then(|t| t.parse().ok())
}

/// Every step has a `timeout-minutes` of its own and the job's is at least their sum.
fn time(release: &str, steps: &[workflows::Step], out: &mut Vec<String>) -> Result<(), String> {
    let mut sum = 0;
    for step in steps {
        match minutes(step.timeout_minutes.as_deref()) {
            Some(n) if n > 0 => sum += n,
            _ => out.push(format!(
                "the step {:?} of `{GAMMA}` has no `timeout-minutes` of its own: if it hangs, only the job's timeout would cut it",
                name_of(step)
            )),
        }
    }
    let job = workflows::job_scalar(release, GAMMA, "timeout-minutes")?;
    if minutes(job.as_deref()).is_none_or(|n| n < sum) {
        out.push(format!(
            "the `timeout-minutes` of `{GAMMA}` ({}) is less than the sum of its steps' ({sum})",
            job.as_deref().unwrap_or("none")
        ));
    }
    Ok(())
}
