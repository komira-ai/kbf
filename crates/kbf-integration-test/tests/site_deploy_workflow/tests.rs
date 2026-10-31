//! Tests of the site deploy lint. The real workflow file is the control (no problem); each other test
//! changes one thing in it and names the rule it holds.

use super::*;

fn real() -> String {
    std::fs::read_to_string(crate::repo_root().join(FILE)).expect("the site publish workflow")
}

/// The problems with `from` replaced by `to` once in the real file, which must hold `from` once.
fn plant(from: &str, to: &str) -> Vec<String> {
    let text = real();
    assert_eq!(
        text.matches(from).count(),
        1,
        "the plant `{from}` must occur once in the real workflow"
    );
    let planted = text.replacen(from, to, 1);
    assert_ne!(planted, text);
    problems(&planted)
}

fn refused(found: &[String], want: &str) {
    assert!(
        found.iter().any(|m| m.contains(want)),
        "want `{want}`: {found:#?}"
    );
}

#[test]
fn the_real_workflow_keeps_every_rule() {
    // Catches: a workflow that drifts from the rules (or a rule that stops matching the file and
    // passes vacuously): the control for every planted case below.
    assert_eq!(problems(&real()), Vec::<String>::new());
}

#[test]
fn nothing_but_a_push_to_main_or_a_manual_run_starts_it() {
    // Catches: the deploy reachable from a pull request (either trigger), a schedule, a tag push, a
    // second branch, or a push filter that is neither branches nor paths.
    let on = "  workflow_dispatch:\n";
    for event in [
        "pull_request",
        "pull_request_target",
        "schedule",
        "workflow_run",
        "release",
    ] {
        let found = plant(on, &format!("{on}  {event}:\n"));
        refused(&found, "not on `push` and `workflow_dispatch` alone");
    }
    refused(
        &plant(on, ""),
        "not on `push` and `workflow_dispatch` alone",
    );
    refused(
        &plant("branches: [main]", "branches: [main, release]"),
        "not exactly [\"main\"]",
    );
    refused(
        &plant("branches: [main]", "branches: ['**']"),
        "not exactly [\"main\"]",
    );
    refused(
        &plant(
            "    branches: [main]\n",
            "    branches: [main]\n    tags: ['v*']\n",
        ),
        "the filter `tags`",
    );
    refused(
        &plant("    branches: [main]\n", "    branches-ignore: [x]\n"),
        "the filter `branches-ignore`",
    );
}

#[test]
fn the_deploy_job_runs_in_its_environment_on_main_only() {
    // Catches: the deploy without its protected environment (no reviewers, no branch rule, no
    // variables), in another one, a condition that lets a branch or a pull request run it, and a
    // deploy that does not wait for the build.
    let env = "    environment: prod-site-tierstore\n";
    refused(&plant(env, ""), "environment is None");
    refused(
        &plant(env, "    environment: prod\n"),
        "environment is Some(\"prod\")",
    );
    refused(
        &plant(" && github.ref == 'refs/heads/main' }}", " }}"),
        "the deploy job's `if:`",
    );
    refused(
        &plant(
            "(github.event_name == 'push' || github.event_name == 'workflow_dispatch')",
            "github.event_name != 'x'",
        ),
        "the deploy job's `if:`",
    );
    refused(
        &plant("    needs: build\n", ""),
        "does not need exactly the build job",
    );
}

#[test]
fn only_the_deploy_job_holds_the_token_and_it_holds_nothing_else() {
    // Catches: `id-token: write` in the job that runs the repository's code, the deploy job gaining
    // a repository or other right, and the build job losing its read-only grant.
    let build = "    permissions:\n      contents: read\n";
    refused(
        &plant(
            build,
            "    permissions:\n      contents: read\n      id-token: write\n",
        ),
        "the build job's permissions",
    );
    refused(
        &plant(build, "    permissions:\n      contents: write\n"),
        "the build job's permissions",
    );
    refused(&plant(build, ""), "the build job's permissions");
    let deploy = "    permissions:\n      id-token: write\n";
    refused(
        &plant(
            deploy,
            "    permissions:\n      id-token: write\n      contents: write\n",
        ),
        "the deploy job's permissions",
    );
    refused(
        &plant(deploy, "    permissions:\n      id-token: read\n"),
        "the deploy job's permissions",
    );
    refused(&plant(deploy, ""), "the deploy job's permissions");
    let build_env = "    timeout-minutes: 15\n    permissions:\n      contents: read\n";
    refused(
        &plant(
            build_env,
            "    timeout-minutes: 15\n    environment: prod-site-tierstore\n    permissions:\n      contents: read\n",
        ),
        "the build job has an environment",
    );
}

#[test]
fn the_role_the_bucket_and_the_distribution_come_from_variables_never_a_key() {
    // Catches: the role taken from a secret or written into the file, a bucket or distribution that
    // is not a variable, a long-lived key passed to the job (a secret or a key variable), and a
    // setting that stops reaching the program as its flag.
    let role = "${{ vars.TIERSTORE_SITE_PUBLISH_ROLE_ARN }}";
    refused(
        &plant(role, "${{ secrets.TIERSTORE_SITE_PUBLISH_ROLE_ARN }}"),
        "ROLE_ARN is Some(\"${{ secrets.",
    );
    refused(
        &plant(role, "arn:aws:iam::000000000000:role/x"),
        "the publish step's ROLE_ARN",
    );
    refused(
        &plant("${{ vars.TIERSTORE_SITE_BUCKET }}", "a-bucket"),
        "the publish step's BUCKET",
    );
    refused(
        &plant(
            "${{ vars.TIERSTORE_SITE_DISTRIBUTION_ID }}",
            "${{ vars.OTHER }}",
        ),
        "the publish step's DISTRIBUTION_ID",
    );
    refused(
        &plant(
            "          ROLE_ARN: ${{ vars.TIERSTORE_SITE_PUBLISH_ROLE_ARN }}\n",
            "          ROLE_ARN: ${{ vars.TIERSTORE_SITE_PUBLISH_ROLE_ARN }}\n          AWS_ACCESS_KEY_ID: x\n",
        ),
        "names `AWS_ACCESS_KEY_ID`",
    );
    refused(
        &plant(
            "          ROLE_ARN: ${{ vars.TIERSTORE_SITE_PUBLISH_ROLE_ARN }}\n",
            "          ROLE_ARN: ${{ vars.TIERSTORE_SITE_PUBLISH_ROLE_ARN }}\n          KEY: ${{ secrets.KEY }}\n",
        ),
        "names `secrets.`",
    );
    refused(
        &plant("--role-arn \"$ROLE_ARN\"", "--role-arn some-role"),
        "lacks `--role-arn \"$ROLE_ARN\"`",
    );
    refused(
        &plant("--bucket \"$BUCKET\"", "--bucket some-bucket"),
        "lacks `--bucket \"$BUCKET\"`",
    );
    refused(
        &plant(
            "permissions: {}\n",
            "permissions: {}\n\nenv:\n  AWS_SECRET_ACCESS_KEY: x\n",
        ),
        "names `AWS_SECRET_ACCESS_KEY`",
    );
}

#[test]
fn the_site_is_built_and_tested_before_anything_is_uploaded() {
    // Catches: an upload that follows a build that skipped its tests or its type check, the steps in
    // another order, a skippable or failure-tolerant step, an install with lifecycle scripts, and
    // an artifact that is uploaded before the build is done.
    refused(
        &plant("      - run: npm test\n", ""),
        "does not run `npm test`",
    );
    refused(
        &plant("      - run: npm run typecheck\n", ""),
        "does not run `npm run typecheck`",
    );
    refused(
        &plant(
            "      - run: npm test\n      - run: npm run build\n",
            "      - run: npm run build\n      - run: npm test\n",
        ),
        "does not run `npm run build`",
    );
    refused(
        &plant("npm ci --ignore-scripts --no-audit --no-fund", "npm ci"),
        "does not run `npm ci --ignore-scripts --no-audit --no-fund`",
    );
    refused(
        &plant(
            "      - run: npm test\n",
            "      - run: npm test\n        if: ${{ always() }}\n",
        ),
        "an `if:` or `continue-on-error:`",
    );
    refused(
        &plant(
            "      - run: npm test\n",
            "      - run: npm test\n        continue-on-error: true\n",
        ),
        "an `if:` or `continue-on-error:`",
    );
    let upload =
        "      - uses: actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a # v7.0.1\n";
    refused(
        &plant(
            "      - run: npm ci --ignore-scripts --no-audit --no-fund\n",
            &format!(
                "{upload}        with:\n          name: early\n          path: x\n      - run: npm ci --ignore-scripts --no-audit --no-fund\n"
            ),
        ),
        "comes before the build and the tests",
    );
}

#[test]
fn the_deploy_job_runs_only_the_program_that_was_built_and_tested() {
    // Catches: the deploy job gaining a checkout, a package install or build of its own beside the
    // token, a command that is not the publish program, a step after the publish, or a publish
    // that does not follow the download.
    let download = "      - uses: actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c # v8.0.1\n";
    refused(
        &plant(
            download,
            "      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1\n      - uses: actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c # v8.0.1\n",
        ),
        "checks out the repository",
    );
    refused(
        &plant(
            "      - name: Upload the site and invalidate the distribution\n",
            "      - run: npm ci\n      - name: Upload the site and invalidate the distribution\n",
        ),
        "its only command is the publish program",
    );
    refused(
        &plant("run: node publish/", "run: node other/"),
        "its only command is the publish program",
    );
    let mut text = real();
    text.push_str("      - run: node publish/after.js\n");
    refused(&problems(&text), "end with exactly one `run:` step");
}

#[test]
fn a_deploy_in_progress_is_never_cancelled() {
    // Catches: `cancel-in-progress` turned on (a later push would kill an upload half done), a group
    // that varies per ref (two deploys could overlap), and no concurrency at all.
    refused(
        &plant("cancel-in-progress: false", "cancel-in-progress: true"),
        "the concurrency is",
    );
    refused(
        &plant(
            "cancel-in-progress: false",
            "cancel-in-progress: ${{ github.event_name == 'pull_request' }}",
        ),
        "the concurrency is",
    );
    refused(
        &plant(
            "group: tierstore-site-publish",
            "group: tierstore-site-publish-${{ github.ref }}",
        ),
        "the concurrency is",
    );
    refused(
        &plant(
            "concurrency:\n  group: tierstore-site-publish\n  cancel-in-progress: false\n",
            "",
        ),
        "the concurrency is None",
    );
}

#[test]
fn a_workflow_with_other_jobs_is_refused() {
    // Catches: the token and the build merged into one job, or a third job added beside them.
    refused(&plant("  deploy:\n", "  ship:\n"), "the jobs are");
    refused(&plant("  build:\n", "  compile:\n"), "the jobs are");
}
