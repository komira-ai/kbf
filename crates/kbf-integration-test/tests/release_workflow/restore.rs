//! The restore of `gamma` (docs/design/gamma-cicd.md, G1): what the job must keep so that a
//! red gamma sets the farm back to the version the release replaced.
//!
//! - **The step.** One step runs `cargo xtask deploy-farm restore`, as the job's last step,
//!   under exactly [`RESTORE_IF`] (it runs when an earlier step failed, and a green job skips
//!   it). It is given the coordinator's address and CA from the same variables as the deploy,
//!   the version the deploy was given, and the run id and attempt as its run ([`RESTORE_ENV`]),
//!   through `env:` and nothing else, and its words are exactly those four flags, each once
//!   ([`RESTORE_FLAGS`]): no `--service` that would restore another service, and no repeated
//!   flag.
//! - **Time.** Every step of the job has a `timeout-minutes` of its own; the deploy is given
//!   `--deadline-min` explicitly and its step's timeout is at least that and
//!   covers [`xtask::deploy_farm::deploy_bound`] of that deadline and of its `--every-secs`; the
//!   restore's covers [`xtask::deploy_farm::restore::bound`]; each with [`START_MIN`] more to
//!   start the program. The bounds are the clients' own: their call timeout, their tries and
//!   their pauses, the token's call included. And the job's
//!   `timeout-minutes` is at least the sum of the steps'. A step that hangs is then cut by its
//!   own timeout and fails, the restore runs, and the job's timeout cannot cut the restore.

use std::time::Duration;

use xtask::deploy_farm::{EVERY_SECS, deploy_bound, restore};

use super::{GAMMA, NONCE, command, flag_is, lines, value, workflows};

/// The condition of the restore step: an earlier step of the job failed.
pub const RESTORE_IF: &str = "${{ failure() }}";

/// The verb of the restore step.
const RESTORE: &str = "cargo xtask deploy-farm restore ";

/// The minutes a step needs, beyond the bound of the verb it runs, to start the program
/// (`cargo xtask`, built by an earlier step of the job).
pub const START_MIN: u64 = 1;

/// The whole minutes a step of a verb whose longest run is `bound` needs: the bound rounded
/// up, and [`START_MIN`].
pub fn step_minutes(bound: std::time::Duration) -> u64 {
    bound.as_secs().div_ceil(60) + START_MIN
}

/// The words of the restore, exactly: each flag once, with its variable.
pub const RESTORE_FLAGS: [(&str, &str); 4] = [
    ("--url", "$URL"),
    ("--ca-pem", "$CA_PEM"),
    ("--version", "$RELEASE"),
    ("--run", "$RUN"),
];

/// The `env:` of the restore step, exactly: the deploy's address and CA variables, the version
/// the deploy was given, and the run.
pub const RESTORE_ENV: [(&str, &str); 4] = [
    ("URL", "${{ vars.KBF_COORDINATOR_URL }}"),
    ("CA_PEM", "${{ vars.KBF_COORDINATOR_CA }}"),
    ("RELEASE", "${{ steps.farm-version.outputs.version }}"),
    ("RUN", NONCE),
];

/// The deploy step of `gamma` with its words, found by its first flag: `deploy-farm restore`
/// is the job's other use of the verb. A problem when there is none.
pub fn deploy_step<'a>(
    steps: &'a [workflows::Step],
    out: &mut Vec<String>,
) -> Option<(usize, &'a workflows::Step, Vec<&'a str>)> {
    let Some((at, step, rest)) = command(steps, "cargo xtask deploy-farm --url ") else {
        out.push(format!(
            "the job `{GAMMA}` does not run `cargo xtask deploy-farm` (its first flag `--url`)"
        ));
        return None;
    };
    // The flag the prefix took is put back in front of the words.
    Some((at, step, std::iter::once("--url").chain(rest).collect()))
}

/// `code`, the workflow's text without comments, with the one `failure()` the halting rule
/// allows taken out: the condition of `gamma`'s last step when that step is the restore.
pub fn without_its_condition(release: &str, code: String) -> Result<String, String> {
    let steps = workflows::job_steps(release, GAMMA)?;
    let last = steps.len().checked_sub(1);
    Ok(
        if last.is_some_and(|last| is_restore(GAMMA, &steps, last)) {
            code.replacen(RESTORE_IF, "", 1)
        } else {
            code
        },
    )
}

fn runs_restore(step: &workflows::Step) -> bool {
    lines(step).any(|l| l.starts_with(RESTORE))
}

/// Whether the step at `at` of `job`'s `steps` is the restore as the halting rule allows it:
/// `gamma`'s last step, running the restore, under exactly [`RESTORE_IF`].
pub fn is_restore(job: &str, steps: &[workflows::Step], at: usize) -> bool {
    job == GAMMA
        && at + 1 == steps.len()
        && runs_restore(&steps[at])
        && steps[at].condition.as_deref() == Some(RESTORE_IF)
}

/// The rules of the restore and of the job's time, given the words of the deploy step.
pub fn rules(
    release: &str,
    steps: &[workflows::Step],
    deploy: &[&str],
    out: &mut Vec<String>,
) -> Result<(), String> {
    time(release, steps, deploy, out)?;
    let at: Vec<usize> = (0..steps.len())
        .filter(|&i| runs_restore(&steps[i]))
        .collect();
    let Some(&last) = at.last() else {
        out.push(format!(
            "the job `{GAMMA}` does not run `{}`: a red gamma would leave the release desired",
            RESTORE.trim_end()
        ));
        return Ok(());
    };
    if at.len() > 1 {
        out.push(format!(
            "`{GAMMA}` runs `{}` {} times, not once",
            RESTORE.trim_end(),
            at.len()
        ));
    }
    if last + 1 != steps.len() {
        out.push(format!(
            "`{}` is not the last step of `{GAMMA}`: a step after it could fail with nothing set back",
            RESTORE.trim_end()
        ));
    }
    let step = &steps[last];
    if step.condition.as_deref() != Some(RESTORE_IF) {
        out.push(format!(
            "`{}` is not under exactly `if: {RESTORE_IF}`: it must run when an earlier step failed, and only then",
            RESTORE.trim_end()
        ));
    }
    let words: Vec<&str> = lines(step)
        .find_map(|l| l.strip_prefix(RESTORE))
        .unwrap_or_default()
        .split_whitespace()
        .map(|w| w.trim_matches('"'))
        .collect();
    let verb = RESTORE.trim_end();
    for (flag, want) in RESTORE_FLAGS {
        flag_is(&words, verb, flag, want, out);
    }
    let want: Vec<&str> = RESTORE_FLAGS.iter().flat_map(|(f, v)| [*f, *v]).collect();
    if words != want {
        out.push(format!(
            "`{verb}` is given {words:?}, not exactly {want:?}: another flag (a `--service`, a repeated flag) would restore something else or nothing"
        ));
    }
    let mut given: Vec<(&str, &str)> = step
        .env
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    given.sort_unstable();
    let mut want = RESTORE_ENV.to_vec();
    want.sort_unstable();
    if given != want {
        out.push(format!(
            "the step of `{GAMMA}` that runs `{verb}` is given {given:?}, not exactly {want:?}: the deploy's address, CA and version, and the run"
        ));
    }
    Ok(())
}

/// The minutes of a step's or the job's `timeout-minutes`, if it is a whole number.
fn minutes(text: Option<&str>) -> Option<u64> {
    text.and_then(|t| t.parse().ok())
}

/// The time rules of the job (see the module's documentation).
fn time(
    release: &str,
    steps: &[workflows::Step],
    deploy: &[&str],
    out: &mut Vec<String>,
) -> Result<(), String> {
    let mut sum = 0;
    for step in steps {
        let name = step
            .name
            .as_deref()
            .or(step.uses.as_deref())
            .or(step.run.as_deref());
        match minutes(step.timeout_minutes.as_deref()) {
            Some(n) if n > 0 => sum += n,
            _ => out.push(format!(
                "the step {:?} of `{GAMMA}` has no `timeout-minutes` of its own: if it hangs, the job's timeout would cut the restore",
                name.unwrap_or("?")
            )),
        }
    }
    let step_of = |prefix: &str| {
        steps
            .iter()
            .find(|s| lines(s).any(|l| l.starts_with(prefix)))
            .and_then(|s| minutes(s.timeout_minutes.as_deref()))
    };
    let every = value(deploy, "--every-secs")
        .and_then(|s| s.parse().ok())
        .unwrap_or(EVERY_SECS);
    let every = Duration::from_secs(every);
    match value(deploy, "--deadline-min").and_then(|m| m.parse::<u64>().ok()) {
        None => out.push(
            "`cargo xtask deploy-farm` is not given `--deadline-min <minutes>`: its step's timeout must leave room after the deadline".to_owned(),
        ),
        Some(deadline) => {
            let need = step_minutes(deploy_bound(Duration::from_secs(deadline * 60), every));
            if step_of("cargo xtask deploy-farm --url ").is_some_and(|n| n < need) {
                out.push(format!(
                    "the deploy step's `timeout-minutes` is less than {need}, the longest the deploy with a deadline of {deadline} minutes can run (its reads and POST tried again, the token's calls included) and {START_MIN} to start: it would be cut before it ends by itself"
                ));
            }
        }
    }
    // The restore is given no `--every-secs` (its flags are exactly four): its default.
    let need = step_minutes(restore::bound(Duration::from_secs(EVERY_SECS)));
    if step_of(RESTORE).is_some_and(|n| n < need) {
        out.push(format!(
            "the restore step's `timeout-minutes` is less than {need}, the longest the restore can run (two reads and a POST, each tried again, the token's calls included) and {START_MIN} to start: it could be cut before its tries end"
        ));
    }
    let job = workflows::job_scalar(release, GAMMA, "timeout-minutes")?;
    if minutes(job.as_deref()).is_none_or(|n| n < sum) {
        out.push(format!(
            "the `timeout-minutes` of `{GAMMA}` ({}) is less than the sum of its steps' ({sum}): the job's timeout could cut the restore",
            job.as_deref().unwrap_or("none")
        ));
    }
    Ok(())
}
