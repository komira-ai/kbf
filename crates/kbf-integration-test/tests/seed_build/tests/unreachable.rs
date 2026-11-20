//! Spoiled copies of the seed jobs that could reach a farm: a stored variable or secret read,
//! or a private network joined.

use super::*;

/// The problems of the real workflows with the step `build` of the seed job `job` given the
/// `env:` entry `env` before its `run:`.
fn with_step_env(job: &str, build: &str, env: &str) -> Vec<String> {
    let run = build.replacen("- run:", "run:", 1);
    spoil_artifacts(
        job,
        build,
        &format!("- env:\n          {env}\n        {run}"),
    )
}

#[test]
fn a_seed_job_that_reads_a_stored_variable_or_secret_is_refused() {
    // Catches: a farm address or a network key handed to a seed job from the repository's
    // settings, in a step's `env:`, its condition (an expression without `${{ }}`), a quoted
    // run line, or the workflow's `env:`, in any case and either way of indexing.
    for (job, build) in KBF_BUILDS {
        for (env, ctx) in [
            ("FARM: ${{ vars.FARM_ADDRESS }}", "vars"),
            ("FARM: ${{ Vars.FARM_ADDRESS }}", "vars"),
            ("FARM: \"${{ vars['FARM_ADDRESS'] }}\"", "vars"),
            ("KEY: ${{ secrets.TS_AUTHKEY }}", "secrets"),
            ("ALL: ${{ toJSON(secrets) }}", "secrets"),
            ("ALL: ${{ toJSON( vars ) }}", "vars"),
        ] {
            red(
                &with_step_env(job, build, env),
                &format!("the seed job `{job}` reads the `{ctx}` context"),
            );
        }
        let line = build.trim_start_matches("- run: ").trim_end();
        for (to, ctx) in [
            (
                format!("- if: vars.FARM_ADDRESS != ''\n        run: {line}\n"),
                "vars",
            ),
            (
                format!("- run: echo '${{{{ secrets.TS_AUTHKEY }}}}' && {line}\n"),
                "secrets",
            ),
        ] {
            red(
                &spoil_artifacts(job, build, &to),
                &format!("the seed job `{job}` reads the `{ctx}` context"),
            );
        }
    }
    let mut w = real();
    w.artifacts = w.artifacts.replacen(
        "env:\n  CARGO_TERM_COLOR: always\n",
        "env:\n  CARGO_TERM_COLOR: always\n  FARM_ADDRESS: ${{ vars.FARM_ADDRESS }}\n",
        1,
    );
    assert_ne!(w.artifacts, read(ARTIFACTS), "the workflow env is spoiled");
    let found = w.problems();
    for job in SEED_JOBS {
        red(
            &found,
            &format!("the seed job `{job}` reads the `vars` context"),
        );
    }
}

#[test]
fn a_seed_job_that_joins_a_private_network_is_refused() {
    // Catches: a seed job given a route to a farm: the tailnet action, or a VPN brought up
    // by a command.
    for (job, build) in KBF_BUILDS {
        for (to, route) in [
            (
                format!(
                    "- uses: tailscale/github-action@0000000000000000000000000000000000000000 # v4\n      {build}"
                ),
                "tailscale",
            ),
            (
                format!("- run: sudo TailScale up\n      {build}"),
                "tailscale",
            ),
            (
                format!("- run: sudo wg-quick up wg0\n      {build}"),
                "wg-quick",
            ),
        ] {
            red(
                &spoil_artifacts(job, build, &to),
                &format!("the seed job `{job}` names `{route}`"),
            );
        }
    }
}

#[test]
fn every_route_a_seed_job_may_not_name_is_refused_alone() {
    // Catches: an entry of `FARM_ROUTES` that no longer refuses the scalar holding it.
    let (job, build) = KBF_BUILDS[1];
    for route in FARM_ROUTES {
        red(
            &with_step_env(job, build, &format!("SEED_SPOIL: \"x-{route}-y\"")),
            &format!("the seed job `{job}` names `{route}`"),
        );
    }
}

#[test]
fn every_stored_context_is_refused_alone() {
    // Catches: an entry of `STORED_CONTEXTS` that no longer refuses the scalar reading it.
    let (job, build) = KBF_BUILDS[1];
    for ctx in STORED_CONTEXTS {
        red(
            &with_step_env(job, build, &format!("SEED_SPOIL: ${{{{ {ctx}.X }}}}")),
            &format!("the seed job `{job}` reads the `{ctx}` context"),
        );
    }
}

#[test]
fn a_context_read_is_told_from_words_like_it() {
    // Catches: the context check refusing an innocent scalar (a property or a variable named
    // like a context, prose ending a sentence with the word), or missing a read at either
    // end of the scalar.
    for s in [
        "${{ vars.FARM }}",
        "vars.FARM",
        "x ${{ secrets[format('{0}', 'K')] }}",
        "toJSON(secrets)",
        "SECRETS.K",
    ] {
        assert!(stored_context(s).is_some(), "`{s}` reads a stored context");
    }
    for s in [
        "${{ github.event.inputs.vars.x }}",
        "${{ env.secrets_dir }}",
        "$secrets.txt",
        "envvars.x",
        "my-vars.x",
        "The build holds no secrets.",
        "no vars",
        "vars",
        "secrets. Then",
        "",
    ] {
        assert_eq!(stored_context(s), None, "`{s}` reads no stored context");
    }
}
