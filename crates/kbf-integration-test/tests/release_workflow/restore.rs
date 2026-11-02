//! The restore of `gamma` (docs/design/gamma-cicd.md, G1): what the job must keep so that a
//! red gamma sets the farm back to the version the release replaced.
//!
//! - **The step.** One step runs `cargo xtask deploy-farm restore`, as the job's last step,
//!   under exactly [`RESTORE_IF`] (it runs when an earlier step failed, and a green job skips
//!   it). It is given the coordinator's address and CA from the same variables as the deploy,
//!   the version the deploy was given, and the run id and attempt as its run ([`RESTORE_ENV`]),
//!   through `env:` and nothing else.
//! - **Time.** The deploy is given `--deadline-min` explicitly, and the job's `timeout-minutes`
//!   is at least that deadline and [`ROOM_MIN`] more, so the timeout does not cut the job
//!   before the proof and the restore.

use super::{GAMMA, NONCE, command, flag_is, lines, value, workflows};

/// The condition of the restore step: an earlier step of the job failed.
pub const RESTORE_IF: &str = "${{ failure() }}";

/// The verb of the restore step.
const RESTORE: &str = "cargo xtask deploy-farm restore ";

/// The minutes `gamma` needs beyond the deploy's deadline: the build of xtask, the fetch of
/// the version and the reads before the deadline starts, and the proof and the restore after
/// it.
pub const ROOM_MIN: u64 = 30;

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
    let deadline = value(deploy, "--deadline-min").and_then(|m| m.parse::<u64>().ok());
    let timeout = workflows::job_scalar(release, GAMMA, "timeout-minutes")?;
    match (deadline, timeout.as_deref().map(str::parse::<u64>)) {
        (None, _) => out.push(
            "`cargo xtask deploy-farm` is not given `--deadline-min <minutes>`: the job's timeout must leave room after the deadline for the proof and the restore".to_owned(),
        ),
        (Some(deadline), Some(Ok(timeout))) if timeout >= deadline + ROOM_MIN => {}
        (Some(deadline), _) => out.push(format!(
            "the `timeout-minutes` of `{GAMMA}` ({}) is not at least the deploy's deadline ({deadline}) and {ROOM_MIN} minutes more: the timeout would cut the proof or the restore",
            timeout.as_deref().unwrap_or("none")
        )),
    }
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
    flag_is(&words, verb, "--url", "$URL", out);
    flag_is(&words, verb, "--ca-pem", "$CA_PEM", out);
    flag_is(&words, verb, "--version", "$RELEASE", out);
    flag_is(&words, verb, "--run", "$RUN", out);
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
