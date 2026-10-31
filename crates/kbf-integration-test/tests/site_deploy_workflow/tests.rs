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
    let build = "    permissions:\n      contents: read\n    defaults:\n";
    refused(
        &plant(
            build,
            "    permissions:\n      contents: read\n      id-token: write\n    defaults:\n",
        ),
        "the build job's permissions",
    );
    refused(
        &plant(
            build,
            "    permissions:\n      contents: write\n    defaults:\n",
        ),
        "the build job's permissions",
    );
    refused(
        &plant(build, "    defaults:\n"),
        "the build job's permissions",
    );
    let deploy = "    permissions:\n      contents: read\n      id-token: write\n";
    refused(
        &plant(
            deploy,
            "    permissions:\n      contents: read\n      id-token: write\n      packages: write\n",
        ),
        "the deploy job's permissions",
    );
    refused(
        &plant(
            deploy,
            "    permissions:\n      contents: write\n      id-token: write\n",
        ),
        "the deploy job's permissions",
    );
    refused(
        &plant(
            deploy,
            "    permissions:\n      contents: read\n      id-token: read\n",
        ),
        "the deploy job's permissions",
    );
    refused(
        &plant(deploy, "    permissions:\n      id-token: write\n"),
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
fn the_deploy_job_runs_the_commits_own_source_and_nothing_built() {
    // Catches: the program that holds the credentials coming from the build job's artifact (which ran
    // dependency code and could rewrite it), a checkout of another ref or one that keeps the token, a
    // checkout wider than the program and the Node version file, an install or build beside the
    // token, any step beyond the checkout, setup-node, download and the one command, and an artifact
    // that carries more than the pages.
    let reason =
        "not exactly a checkout, a setup-node, a download of the pages and the one publish command";
    refused(
        &plant(
            "      - uses: actions/download-artifact@",
            "      - run: npm ci\n      - uses: actions/download-artifact@",
        ),
        reason,
    );
    refused(
        &plant(
            "      - uses: actions/download-artifact@",
            "      - uses: actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c # v8.0.1\n        with:\n          name: tierstore-site\n          path: more\n      - uses: actions/download-artifact@",
        ),
        reason,
    );
    refused(
        &plant("          ref: ${{ github.sha }}\n", ""),
        "has `ref` None",
    );
    refused(
        &plant(
            "          ref: ${{ github.sha }}\n",
            "          ref: main\n",
        ),
        "has `ref`",
    );
    refused(
        &plant(
            "          persist-credentials: false\n          sparse-checkout: |\n",
            "          persist-credentials: true\n          sparse-checkout: |\n",
        ),
        "has `persist-credentials`",
    );
    refused(
        &plant(
            "            crates/tierstore/site/tools/publish.ts\n",
            "            crates/tierstore/site/tools/publish.ts\n            crates/tierstore/site/package.json\n",
        ),
        "the deploy job's checkout takes",
    );
    refused(
        &plant("            crates/tierstore/site/tools/publish.ts\n", ""),
        "the deploy job's checkout takes",
    );
    refused(
        &plant("          sparse-checkout-cone-mode: false\n", ""),
        "has `sparse-checkout-cone-mode` None",
    );
    refused(
        &plant(
            "          name: tierstore-site\n          path: dist\n",
            "          name: tierstore-site\n          path: out\n",
        ),
        "has `path`",
    );
    refused(
        &plant(
            "run: node crates/tierstore/site/tools/publish.ts --dist dist ",
            "run: node publish/build/tools/tools/publish.js --dist dist ",
        ),
        "its only command is `node crates/tierstore/site/tools/publish.ts",
    );
    refused(
        &plant(
            "run: node crates/tierstore/site/tools/publish.ts --dist dist ",
            "run: npx tsx crates/tierstore/site/tools/publish.ts --dist dist ",
        ),
        "its only command is `node crates/tierstore/site/tools/publish.ts",
    );
    // The artifact carries the pages only.
    refused(
        &plant(
            "          path: crates/tierstore/site/dist\n",
            "          path: crates/tierstore/site\n",
        ),
        "the build job's artifact path",
    );
}

fn publish_ts() -> String {
    std::fs::read_to_string(crate::repo_root().join("crates/tierstore/site/tools/publish.ts"))
        .expect("publish.ts")
}

fn site_tsconfig() -> String {
    std::fs::read_to_string(crate::repo_root().join("crates/tierstore/site/tsconfig.json"))
        .expect("tsconfig.json")
}

#[test]
fn the_program_that_holds_the_credentials_imports_only_node_builtins() {
    // Catches: publish.ts gaining an import of a package or of another file of the site (the deploy
    // job checks out nothing but this file, installs nothing, and runs it as it is), a bare built-in
    // name (`fs`) that is not the `node:` spelling, a `require` or dynamic `import()`, a side-effect
    // import, a re-export, a multi-line import, and a tsconfig that stops forbidding syntax Node cannot
    // strip. Control: the real file and tsconfig.
    let real = publish_ts();
    let tsconfig = site_tsconfig();
    assert_eq!(source_problems(&real, &tsconfig), Vec::<String>::new());
    assert!(
        specifiers(&real).len() >= 4,
        "the scan reads the real imports"
    );
    for line in [
        "import { x } from './check-dist';",
        "import fs from 'fs';",
        "import x from \"left-pad\";",
        "const m = require('y');",
        "const m = await import('z');",
        "import {\n  a,\n} from 'pkg';",
        "import 'pkg';",
        "export * from './o';",
        "import { readFileSync } from 'node:fs'; import y from 'y';",
    ] {
        let found = source_problems(&format!("{real}\n{line}\n"), &tsconfig);
        assert!(
            found.iter().any(|m| m.contains("only `node:` built-ins")),
            "{line}: {found:?}"
        );
    }
    assert_eq!(
        source_problems(
            &format!("{real}\nimport {{ x }} from 'node:os';\n"),
            &tsconfig
        ),
        Vec::<String>::new()
    );
    let found = source_problems(&real, &tsconfig.replace("erasableSyntaxOnly", "other"));
    assert!(
        found.iter().any(|m| m.contains("erasableSyntaxOnly")),
        "{found:?}"
    );
    let found = source_problems("// nothing\n", &tsconfig);
    assert!(
        found.iter().any(|m| m.contains("read nothing")),
        "{found:?}"
    );
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
