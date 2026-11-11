//! Tests of the apt rules ([`super::super::apt`]): each changes the real workflow in one place and
//! names the rule it catches.

use super::*;

/// The text of the real workflow with the steps of the job `job` changed by `change`.
fn with_steps(job: &str, change: impl FnOnce(&mut Vec<String>)) -> Vec<String> {
    let text = real();
    let header = format!("\n  {job}:\n");
    let start = text.find(&header).expect("the job") + 1;
    let steps_at =
        start + text[start..].find("    steps:\n").expect("steps") + "    steps:\n".len();
    let end = text[steps_at..]
        .find("\n\n")
        .map_or(text.len(), |n| steps_at + n + 1);
    let body = &text[steps_at..end];
    let mut steps: Vec<String> = Vec::new();
    for line in body.split_inclusive('\n') {
        if line.starts_with("      - ") || steps.is_empty() {
            steps.push(String::new());
        }
        steps.last_mut().expect("a step").push_str(line);
    }
    change(&mut steps);
    let changed = [&text[..steps_at], &steps.concat(), &text[end..]].concat();
    problems(&changed).expect("the changed workflow is read")
}

fn at(steps: &[String], needle: &str) -> usize {
    let hits: Vec<usize> = (0..steps.len())
        .filter(|&i| steps[i].contains(needle))
        .collect();
    assert_eq!(hits.len(), 1, "`{needle}` is in {hits:?} steps");
    hits[0]
}

const ATTEST_ARM: &str = "gh attestation verify \"dist/kbf-native-$COMMIT-linux-arm64.deb\"";
const ATTEST_AMD: &str = "gh attestation verify \"dist/kbf-native-$COMMIT-linux-x86_64.deb\"";

#[test]
fn the_apt_jobs_are_pinned_by_name() {
    // Catches: an apt job renamed, dropped or added without its rule, and the install check
    // folded back into one job with an expression for its runner.
    let names = apt::jobs();
    assert_eq!(names.len(), 21);
    assert_eq!(names[0], "deploy-tool");
    for job in [
        "testing-apt-read",
        "testing-apt-index-2",
        "testing-apt-write-3",
        "check-apt-amd64",
        "check-apt-arm64",
        "stable-apt-read",
        "stable-apt-index-3",
        "stable-apt-write-2",
    ] {
        assert!(names.iter().any(|n| n == job), "{job}");
    }
    red(
        &plant_top("  check-apt-amd64:\n", "  check-apt:\n"),
        "no job `check-apt-amd64`",
    );
    red(
        &plant_top("  stable-apt-write-3:\n", "  stable-apt-write-last:\n"),
        "no job `stable-apt-write-3`",
    );
}

#[test]
fn an_index_job_holds_no_id_token_and_no_role() {
    // Catches: the signing key and the cloud role in one job: an index job with `id-token: write`,
    // in the publish environment, or assuming the role.
    red(
        &plant(
            "testing-apt-index",
            "      contents: read\n",
            "      contents: read\n      id-token: write\n",
        ),
        "the job `testing-apt-index` holds the scopes",
    );
    red(
        &plant(
            "stable-apt-index",
            "    environment: apt-signing\n",
            "    environment: apt-publish\n",
        ),
        "the job `stable-apt-index` has environment",
    );
    red(
        &plant(
            "testing-apt-index-2",
            "          APT_GPG_FINGERPRINT: ${{ vars.APT_GPG_FINGERPRINT }}\n          APT_GPG_PRIVATE_KEY: ${{ secrets.APT_GPG_PRIVATE_KEY }}\n          APT_GPG_PASSPHRASE: ${{ secrets.APT_GPG_PASSPHRASE }}\n        run: tool/xtask channels gate",
            "          APT_GPG_FINGERPRINT: ${{ vars.APT_GPG_FINGERPRINT }}\n          APT_GPG_PRIVATE_KEY: ${{ secrets.APT_GPG_PRIVATE_KEY }}\n          APT_GPG_PASSPHRASE: ${{ secrets.APT_GPG_PASSPHRASE }}\n          ROLE: ${{ vars.APT_PUBLISH_ROLE_ARN }}\n        run: tool/xtask channels gate",
        ),
        "holds the signing key and the cloud role",
    );
    red(
        &plant(
            "testing-apt-read",
            "      contents: read\n      id-token: write\n",
            "      contents: read\n",
        ),
        "the job `testing-apt-read` holds the scopes",
    );
    red(
        &plant(
            "testing-apt-write-2",
            "    environment: apt-publish\n",
            "    environment: apt-signing\n",
        ),
        "the job `testing-apt-write-2` has environment",
    );
}

#[test]
fn no_other_job_holds_id_token() {
    // Catches: `id-token: write` in a job of the apt chain that has no use for it: the tool
    // build and the install checks.
    for job in ["deploy-tool", "check-apt-amd64", "check-apt-arm64"] {
        red(
            &plant(
                job,
                "      contents: read\n",
                "      contents: read\n      id-token: write\n",
            ),
            &format!("the job `{job}` holds"),
        );
    }
}

#[test]
fn a_write_checks_the_files_and_the_attestations_before_it_writes() {
    // Catches: a write, an index or a promotion that uses files nobody checked: the attestation
    // of either .deb replaced by another command, the sums check gone, or the checks moved after
    // the write.
    for job in [
        "testing-apt-write",
        "testing-apt-index",
        "testing-apt-write-3",
    ] {
        for (needle, why) in [
            (ATTEST_ARM, "the attestation check of the arm64 .deb"),
            (ATTEST_AMD, "the attestation check of the amd64 .deb"),
        ] {
            let found = plant(
                job,
                needle,
                &needle.replace("gh attestation verify", "echo"),
            );
            red(&found, why);
        }
        red(
            &plant(
                job,
                "run: sha256sum --check --strict SHA256SUMS",
                "run: sha256sum SHA256SUMS",
            ),
            "the check of the files against SHA256SUMS",
        );
    }
    red(
        &plant(
            "stable-apt-index",
            ATTEST_ARM,
            &ATTEST_ARM.replace("gh attestation verify", "echo"),
        ),
        "the attestation check of the arm64 .deb",
    );
    let found = with_steps("testing-apt-write", |steps| {
        let attest = at(steps, ATTEST_AMD);
        steps[attest] = steps[attest].replace("refs/heads/main", "refs/heads/other");
    });
    red(&found, "the attestation check of the amd64 .deb");
    let found = with_steps("testing-apt-write", |steps| {
        let attest = at(steps, ATTEST_ARM);
        let moved = steps.remove(attest);
        steps.push(moved);
    });
    red(&found, "is out of order");
    let found = with_steps("testing-apt-write", |steps| {
        let attest = at(steps, ATTEST_ARM);
        steps.remove(attest);
    });
    red(&found, "the attestation check of the arm64 .deb");
}

#[test]
fn every_apt_job_starts_with_the_tool_and_the_gate() {
    // Catches: a job without its gate (it would run without its settings), a gate with other
    // settings or requirements, a step after the gate that is not under its output, and a tool
    // that is not the one `deploy-tool` built.
    for job in [
        "testing-apt-read",
        "testing-apt-index",
        "stable-apt-write-2",
    ] {
        let found = with_steps(job, |steps| {
            let gate = at(steps, "channels gate");
            steps.remove(gate);
        });
        red(&found, "the gate on the settings");
    }
    red(
        &plant(
            "testing-apt-write",
            "--require READ_ENABLED --require INDEX_SIGNED",
            "--require READ_ENABLED",
        ),
        "the gate on the settings",
    );
    red(
        &plant("testing-apt-read", " --setting APT_HOSTNAME", ""),
        "the gate on the settings",
    );
    red(
        &plant(
            "testing-apt-index-3",
            "--label testing-apt-index-3",
            "--label testing-apt-index",
        ),
        "the gate on the settings",
    );
    red(
        &plant(
            "stable-apt-read",
            "          TESTING_ENABLED: ${{ needs.testing-apt-read.outputs.enabled }}\n",
            "          TESTING_ENABLED: true\n",
        ),
        "the gate on the settings",
    );
    red(
        &plant(
            "testing-apt-read",
            "        if: ${{ steps.gate.outputs.enabled == 'true' }}\n        env:\n          ROLE_ARN",
            "        env:\n          ROLE_ARN",
        ),
        "the assumption of the apt publish role",
    );
    red(
        &plant(
            "testing-apt-read",
            "        if: ${{ steps.gate.outputs.enabled == 'true' }}\n        env:\n          ROLE_ARN",
            "        if: ${{ always() }}\n        env:\n          ROLE_ARN",
        ),
        "a step of `testing-apt-read`",
    );
    red(
        &plant("testing-apt-read", "name: deploy-tool\n", "name: other\n"),
        "the download of the `deploy-tool` artifact",
    );
    red(
        &plant("testing-apt-write", "chmod +x tool/xtask", "echo tool"),
        "`chmod +x tool/xtask`",
    );
}

#[test]
fn no_job_that_holds_the_role_or_the_key_checks_out_or_builds() {
    // Catches: code from the repository, or a build script, running beside the signing key or the
    // cloud role: a checkout or a `cargo` step in a read, an index or a write job.
    for job in ["testing-apt-read", "testing-apt-index", "stable-apt-write"] {
        let found = with_steps(job, |steps| {
            steps.insert(
                0,
                "      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1\n        with:\n          ref: ${{ github.event.workflow_run.head_sha }}\n          persist-credentials: false\n".to_owned(),
            );
        });
        red(
            &found,
            &format!("the job `{job}` checks out the repository"),
        );
    }
    let found = with_steps("testing-apt-write", |steps| {
        let at = at(steps, "chmod +x");
        steps.insert(
            at,
            "      - run: cargo build -p xtask --locked\n".to_owned(),
        );
    });
    red(&found, "the job `testing-apt-write` builds with `cargo`");
}

#[test]
fn the_signing_key_is_read_by_the_index_jobs_alone_and_only_to_sign() {
    // Catches: the key in a read or a write job, in a step of an index job other than the gate
    // and the sign step, and in a job that is not an apt job.
    let key = "          APT_GPG_PRIVATE_KEY: ${{ secrets.APT_GPG_PRIVATE_KEY }}\n";
    for job in ["testing-apt-read", "testing-apt-write", "stable-apt-read-2"] {
        let found = with_steps(job, |steps| {
            let gate = at(steps, "channels gate");
            steps[gate] = steps[gate].replace("        run:", &format!("{key}        run:"));
        });
        red(&found, &format!("the job `{job}` reads `APT_GPG_*`"));
        red(
            &found,
            &format!("the job `{job}` reads a secret other than"),
        );
    }
    let found = with_steps("testing-apt-index", |steps| {
        let merge = at(steps, "channels apt index");
        steps[merge] = steps[merge].replace("        run:", &format!("{key}        run:"));
    });
    red(
        &found,
        "holds the signing key: only the gate and the sign step may",
    );
    red(
        &plant(
            "bump",
            "          VERSION: ${{ needs.plan.outputs.version }}\n",
            "          VERSION: ${{ needs.plan.outputs.version }}\n          KEY: ${{ vars.APT_GPG_FINGERPRINT }}\n",
        ),
        "the job `bump` reads `APT_GPG_*`",
    );
    red(
        &plant(
            "gamma",
            "          NONCE: ${{ github.run_id }}-${{ github.run_attempt }}\n",
            "          NONCE: ${{ github.run_id }}-${{ github.run_attempt }}\n          ROLE: ${{ vars.APT_PUBLISH_ROLE_ARN }}\n",
        ),
        "the job `gamma` reads `APT_PUBLISH_ROLE_ARN`",
    );
}

#[test]
fn the_conditions_of_the_apt_jobs_are_exact() {
    // Catches: an attempt that runs without the conflict that asks for it, a join that does not
    // survive a skipped attempt (`!cancelled() && !failure()` dropped), a promotion that does not
    // wait for both install checks, a check that runs without a write having landed, and
    // `always()` where `!cancelled()` is.
    red(
        &plant(
            "testing-apt-read-2",
            " && needs.testing-apt-write.outputs.conflict == 'true'",
            "",
        ),
        "the job `testing-apt-read-2` has `if:",
    );
    red(
        &plant("testing-apt-index-2", "!cancelled() && !failure() && ", ""),
        "the job `testing-apt-index-2` has `if:",
    );
    red(
        &plant("testing-apt-index-2", "!cancelled() && ", "always() && "),
        "the workflow uses `always()`",
    );
    red(
        &plant(
            "stable-apt-write",
            " && needs.check-apt-arm64.result == 'success'",
            "",
        ),
        "the job `stable-apt-write` has `if:",
    );
    red(
        &plant(
            "check-apt-arm64",
            " || needs.testing-apt-write-3.outputs.conflict == 'false'",
            "",
        ),
        "the job `check-apt-arm64` has `if:",
    );
    red(
        &plant(
            "testing-apt-write",
            "    if: ${{ github",
            "    if: ${{ !cancelled() && github",
        ),
        "the workflow uses `cancelled()`",
    );
    red(
        &plant(
            "deploy-tool",
            " && needs.plan.outputs.pipeline != 'true'",
            "",
        ),
        "the job `deploy-tool` has `if:",
    );
    red(
        &plant(
            "testing-apt-write-3",
            "needs: [plan, verify, testing-apt-read-3, testing-apt-index-3, deploy-tool]",
            "needs: [plan, verify, testing-apt-read-3, deploy-tool]",
        ),
        "the job `testing-apt-write-3` needs",
    );
    red(
        &plant(
            "stable-apt-index",
            "needs: [plan, verify, check-apt-amd64, check-apt-arm64, stable-apt-read, testing-apt-index, deploy-tool]",
            "needs: [plan, verify, check-apt-amd64, stable-apt-read, testing-apt-index, deploy-tool]",
        ),
        "the job `stable-apt-index` needs",
    );
}

#[test]
fn the_verbs_are_given_the_commit_the_version_and_the_pool_of_their_attempt() {
    // Catches: a write that is not the last attempt's given `--last` or the third without it, a
    // promotion that writes packages, a testing write that does not, an index or promotion not
    // given the commit or the version.
    red(
        &plant("testing-apt-write-3", " --last", ""),
        "the write of the signed index",
    );
    red(
        &plant(
            "testing-apt-write-2",
            "\"$RUNNER_TEMP/apt-next\" --debs",
            "\"$RUNNER_TEMP/apt-next\" --last --debs",
        ),
        "the write of the signed index",
    );
    red(
        &plant(
            "stable-apt-write",
            "--no-pool",
            "--debs dist --commit \"$COMMIT\" --version \"$PACKAGE_VERSION\"",
        ),
        "the write of the signed index",
    );
    red(
        &plant(
            "testing-apt-write",
            "--debs dist --commit \"$COMMIT\" --version \"$PACKAGE_VERSION\"",
            "--no-pool",
        ),
        "the write of the signed index",
    );
    red(
        &plant("testing-apt-index", " --commit \"$COMMIT\"", ""),
        "the merge of the .debs",
    );
    red(
        &plant("stable-apt-index", " --sums dist/SHA256SUMS", ""),
        "the promotion of this release",
    );
    red(
        &plant(
            "testing-apt-index",
            "--version \"$PACKAGE_VERSION\"",
            "--version 1.2.3",
        ),
        "the merge of the .debs",
    );
    red(
        &plant(
            "testing-apt-read",
            "--session-name \"kbf-testing-apt-read-$GITHUB_RUN_ID\"",
            "--session-name \"kbf-$GITHUB_RUN_ID\"",
        ),
        "the assumption of the apt publish role",
    );
    red(
        &plant(
            "stable-apt-write",
            " --token-url \"$ACTIONS_ID_TOKEN_REQUEST_URL\"",
            "",
        ),
        "the assumption of the apt publish role",
    );
}

#[test]
fn the_package_version_comes_from_verify_and_nowhere_else() {
    // Catches: a version from another source: the job env pointing at `plan`, a job that uses it
    // without the env, `verify` not outputting it, its step before the attestations or missing.
    red(
        &plant(
            "testing-apt-index",
            "PACKAGE_VERSION: ${{ needs.verify.outputs.package-version }}",
            "PACKAGE_VERSION: ${{ needs.plan.outputs.version }}",
        ),
        "the job `testing-apt-index` has the environment",
    );
    red(
        &plant(
            "check-apt-amd64",
            "PACKAGE_VERSION: ${{ needs.verify.outputs.package-version }}",
            "PACKAGE_VERSION: 0.1.1",
        ),
        "the job `check-apt-amd64` has the environment",
    );
    red(
        &plant(
            "verify",
            "package-version: ${{ steps.package.outputs.package-version }}",
            "package-version: 0.1.1",
        ),
        "does not output `package-version`",
    );
    red(
        &plant(
            "verify",
            "cargo xtask channels apt version ",
            "cargo xtask other ",
        ),
        "has no unconditional step with `id: package`",
    );
    let found = with_steps("verify", |steps| {
        let package = at(steps, "id: package");
        let moved = steps.remove(package);
        let locate = at(steps, "release locate");
        steps.insert(locate, moved);
    });
    red(&found, "is not after the attestations");
}

#[test]
fn the_install_checks_are_two_literal_jobs_in_a_pinned_container() {
    // Catches: one check with an expression for its runner (the workflow lint refuses it), a
    // check on the wrong architecture, an image that is not pinned by digest, a check that holds
    // a token, and one that does not check out and build the commit.
    red(
        &plant(
            "check-apt-arm64",
            "runs-on: ubuntu-24.04-arm",
            "runs-on: ${{ matrix.runner }}",
        ),
        "the job `check-apt-arm64` runs on",
    );
    red(
        &plant(
            "check-apt-arm64",
            "runs-on: ubuntu-24.04-arm",
            "runs-on: ${{ matrix.runner }}",
        ),
        "workflow lint",
    );
    red(
        &plant(
            "check-apt-arm64",
            "runs-on: ubuntu-24.04-arm",
            "runs-on: ubuntu-24.04",
        ),
        "the job `check-apt-arm64` runs on",
    );
    red(
        &plant("check-apt-amd64", "--arch amd64", "--arch arm64"),
        "the install check in a clean container",
    );
    red(
        &plant(
            "check-apt-amd64",
            "ubuntu@sha256:fe81560990fae86dc4743202b2cb4d3df98d0bcf949173cecef15ce1127ef852",
            "ubuntu:24.04",
        ),
        "the install check in a clean container",
    );
    red(
        &plant("check-apt-amd64", " --commit \"$COMMIT\"", ""),
        "the install check in a clean container",
    );
    let found = with_steps("check-apt-amd64", |steps| {
        let checkout = at(steps, "actions/checkout@");
        steps.remove(checkout);
    });
    red(&found, "the checkout of the commit the pipeline run built");
    red(
        &plant(
            "check-apt-amd64",
            "      - run: cargo build -p xtask --locked\n",
            "      - run: cargo build -p xtask --locked\n      - run: echo done\n",
        ),
        "has a step that is none of its steps",
    );
}

#[test]
fn deploy_tool_builds_the_release_tool_with_no_token_and_uploads_it() {
    // Catches: the tool built in debug, uploaded under another name, built in a job with a token or
    // an environment, or with a secret.
    red(
        &plant(
            "deploy-tool",
            "cargo build -p xtask --locked --release",
            "cargo build -p xtask --locked",
        ),
        "the job `deploy-tool` has no step that is",
    );
    red(
        &plant("deploy-tool", "name: deploy-tool\n", "name: tool\n"),
        "the upload of `target/release/xtask` as `deploy-tool`",
    );
    red(
        &plant(
            "deploy-tool",
            "    permissions:\n",
            "    environment: apt-signing\n    permissions:\n",
        ),
        "the job `deploy-tool` has environment",
    );
}

#[test]
fn a_secret_other_than_the_bots_and_the_keys_is_refused() {
    // Catches: the apt settings' secrets read where they are not: a secret in a check job.
    red(
        &plant(
            "check-apt-amd64",
            "      PACKAGE_VERSION: ${{ needs.verify.outputs.package-version }}\n",
            "      PACKAGE_VERSION: ${{ needs.verify.outputs.package-version }}\n      TOKEN: ${{ secrets.APT_GPG_PASSPHRASE }}\n",
        ),
        "the job `check-apt-amd64` reads a secret other than",
    );
}
