//! Spoiled copies of how the seed files reach the release: who uploads `assets-*`, how the
//! attest jobs and the release's verify download them, and the jobs that need the seed jobs.

use super::*;

#[test]
fn published_files_from_another_job_are_refused() {
    // Catches: a seed job's upload removed, renamed or made skippable (so the release takes
    // files from elsewhere or none), another job of the pipeline's run uploading an assets
    // artifact (by name or by an expression), and an attest job or the release's verify no
    // longer reading the seed job's files.
    for job in SEED_JOBS {
        let upload = format!("name: assets-{job}\n");
        red(
            &spoil_artifacts(job, &upload, &format!("name: built-{job}\n")),
            &format!("does not upload its files as `assets-{job}`"),
        );
        let uses =
            "- uses: actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a # v7.0.1\n";
        red(
            &spoil_artifacts(
                job,
                uses,
                &format!("- if: ${{{{ false }}}}\n        uses: {}", &uses[8..]),
            ),
            &format!("does not upload its files as `assets-{job}`"),
        );
    }
    let upload_step = |name: &str| {
        format!(
            "      - run: cargo xtask sums assemble dist --summary \"$GITHUB_STEP_SUMMARY\"\n      - uses: actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a # v7.0.1\n        with:\n          name: {name}\n          path: dist/\n"
        )
    };
    for name in [
        "assets-linux-x86_64-farm",
        "ASSETS-kbf",
        "${{ inputs.stage }}-files",
        "asset${{ inputs.stage }}",
    ] {
        let from = "      - name: Assemble SHA256SUMS\n        run: cargo xtask sums assemble dist --summary \"$GITHUB_STEP_SUMMARY\"\n";
        red(
            &spoil_artifacts(ALL_SUMS, from, &upload_step(name)),
            &format!("the job `sums` of artifacts.yml uploads `{name}`"),
        );
    }
    let mut w = real();
    let (path, text) = w
        .called
        .iter_mut()
        .find(|(p, _)| p.ends_with("smoke.yml"))
        .expect("the pipeline calls smoke.yml");
    let spoiled = text.replacen(
        "name: beta-b0-${{ github.run_attempt }}",
        "name: assets-darwin-arm64-b0",
        1,
    );
    assert_ne!(*text, spoiled, "smoke.yml is spoiled");
    *text = spoiled;
    let path = path.clone();
    red(
        &w.problems(),
        &format!("of {path} uploads `assets-darwin-arm64-b0`"),
    );
    let mut w = real();
    w.pipeline = in_job(
        &w.pipeline,
        PASSED,
        "    steps:\n",
        "    steps:\n      - uses: actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a # v7.0.1\n        with:\n          name: assets-extra\n          path: x\n",
    );
    red(
        &w.problems(),
        "the job `passed` of pipeline.yml uploads `assets-extra`",
    );

    red(
        &spoil_artifacts(
            "attest-linux",
            "pattern: assets-linux-*\n",
            "pattern: assets-linux-x86_64\n",
        ),
        "the attest job `attest-linux` does not download `assets-linux-arm64`",
    );
    red(
        &spoil_artifacts(
            "attest-darwin-arm64",
            "pattern: assets-darwin-arm64\n",
            "pattern: farm-darwin-arm64\n",
        ),
        "the attest job `attest-darwin-arm64` does not download `assets-darwin-arm64`",
    );
    for (from, to) in [
        ("pattern: assets-*\n", "pattern: farm-*\n"),
        ("pattern: assets-*\n", "pattern: assets-linux-*\n"),
        (
            "          pattern: assets-*\n          path: dist\n          merge-multiple: true\n          run-id: ${{ github.event.workflow_run.id }}\n",
            "          pattern: assets-*\n          path: dist\n          merge-multiple: true\n          run-id: ${{ vars.FARM_RUN }}\n",
        ),
    ] {
        let mut w = real();
        w.release = in_job(&w.release, RELEASE_VERIFY, from, to);
        red(
            &w.problems(),
            "the release's `verify` does not download `assets-darwin-arm64`",
        );
    }
}

#[test]
fn a_release_that_no_longer_needs_the_seed_jobs_is_refused() {
    // Catches: publish no longer needing verify; the pipeline's sign call or `passed` (whose
    // success starts the release) no longer needing the build call, or the calls naming
    // another workflow or stage; a sums job no longer needing a seed job (so the attested
    // sums need not cover its files); a verify job no longer needing its attest job.
    let mut w = real();
    w.release = in_job(
        &w.release,
        RELEASE_PUBLISH,
        "needs: [plan, verify, tag, notes]",
        "needs: [plan, tag, notes]",
    );
    red(
        &w.problems(),
        "the job `publish` of release.yml does not need `verify`",
    );

    for (job, from, to, want) in [
        (
            SIGN_CALL,
            "needs: [ci, artifacts]",
            "needs: [ci]",
            "the job `artifacts-sign` of pipeline.yml does not need `artifacts`",
        ),
        (
            PASSED,
            "needs: [ci, beta, artifacts, artifacts-sign, smoke]",
            "needs: [ci, beta, artifacts-sign, smoke]",
            "the job `passed` of pipeline.yml does not need `artifacts`",
        ),
        (
            PASSED,
            "needs: [ci, beta, artifacts, artifacts-sign, smoke]",
            "needs: [ci, beta, artifacts, smoke]",
            "the job `passed` of pipeline.yml does not need `artifacts-sign`",
        ),
        (
            BUILD_CALL,
            "stage: build",
            "stage: farm",
            "the pipeline's `artifacts` is not the `build` stage",
        ),
        (
            SIGN_CALL,
            "uses: ./.github/workflows/artifacts.yml",
            "uses: ./.github/workflows/ci.yml",
            "the pipeline's `artifacts-sign` is not the `sign` stage",
        ),
    ] {
        let mut w = real();
        w.pipeline = in_job(&w.pipeline, job, from, to);
        red(&w.problems(), want);
    }

    for (job, from, to, want) in [
        (
            "sums-linux",
            "needs: [linux-x86_64, linux-arm64]",
            "needs: [linux-x86_64]",
            "the job `sums-linux` of artifacts.yml does not need `linux-arm64`",
        ),
        (
            "sums-darwin-arm64",
            "needs: [darwin-arm64]",
            "needs: []",
            "the job `sums-darwin-arm64` of artifacts.yml does not need `darwin-arm64`",
        ),
        (
            "sums",
            "needs: [darwin-arm64, linux-x86_64, linux-arm64]",
            "needs: [darwin-arm64, linux-arm64]",
            "the job `sums` of artifacts.yml does not need `linux-x86_64`",
        ),
        (
            "verify-linux",
            "needs: [attest-linux]",
            "needs: []",
            "the job `verify-linux` of artifacts.yml does not need `attest-linux`",
        ),
    ] {
        red(&spoil_artifacts(job, from, to), want);
    }
}

#[test]
fn artifact_patterns_match_as_the_download_action_does() {
    // Catches: the pattern match taking a name a download would not read for one it does,
    // or the reverse, so a rule about who reads the seed files passes on a wrong reader.
    assert!(matches("assets-*", "assets-linux-x86_64"));
    assert!(!matches("assets-linux-*", "ASSETS-linux-arm64"));
    assert!(!matches("Assets-*", "assets-linux-arm64"));
    assert!(matches("assets-darwin-arm64", "assets-darwin-arm64"));
    assert!(matches("*-arm64", "assets-darwin-arm64"));
    assert!(matches("a*s*4", "assets-linux-arm64"));
    assert!(!matches("assets-linux-*", "assets-darwin-arm64"));
    assert!(!matches("assets-darwin-arm64", "assets-darwin-arm64x"));
    assert!(!matches("*-x86_64", "assets-linux-arm64"));
    assert!(!matches("a*rm*rm64", "assets-darwin-arm64"));
    assert!(may_be_assets("assets-linux-x86_64"));
    assert!(may_be_assets("${{ env.NAME }}"));
    assert!(may_be_assets("ass${{ env.NAME }}"));
    assert!(may_be_assets("assets-${{ env.NAME }}"));
    assert!(!may_be_assets("profdata-${{ matrix.core }}"));
    assert!(!may_be_assets("sha256sums-linux"));
}

#[test]
fn uploads_and_downloads_in_every_spelling_are_read() {
    // Catches: an assets artifact uploaded by a sub-action (`upload-artifact/merge`) or under
    // a name with blanks around it; an attest download made skippable, or given
    // `artifact-ids:` or both `name:` and `pattern:` (which the action reads before the
    // pattern); a job of a called workflow calling one this lint never read, or running a
    // local action.
    let from = "      - name: Assemble SHA256SUMS\n        run: cargo xtask sums assemble dist --summary \"$GITHUB_STEP_SUMMARY\"\n";
    for (uses, name) in [
        (
            "actions/upload-artifact/merge@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a",
            "assets-merged",
        ),
        (
            "actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a",
            "\" assets-padded\"",
        ),
    ] {
        let to = format!(
            "      - uses: {uses} # v7.0.1\n        with:\n          name: {name}\n          path: dist/\n"
        );
        red(
            &spoil_artifacts(ALL_SUMS, from, &to),
            "the job `sums` of artifacts.yml uploads `assets-",
        );
    }
    let pattern = "        with:\n          pattern: assets-linux-*\n";
    for to in [
        format!("        if: ${{{{ false }}}}\n{pattern}"),
        format!("{pattern}          artifact-ids: 1\n"),
        format!("{pattern}          name: sha256sums-linux\n"),
    ] {
        red(
            &spoil_artifacts("attest-linux", pattern, &to),
            "the attest job `attest-linux` does not download `assets-linux-x86_64`",
        );
    }
    let mut w = real();
    let (path, text) = w
        .called
        .iter_mut()
        .find(|(p, _)| p.ends_with("smoke.yml"))
        .expect("the pipeline calls smoke.yml");
    text.push_str("  farm:\n    uses: ./.github/workflows/farm-upload.yml\n");
    let path = path.clone();
    red(
        &w.problems(),
        &format!("the job `farm` of {path} calls `./.github/workflows/farm-upload.yml`"),
    );
    let mut w = real();
    w.pipeline = in_job(
        &w.pipeline,
        PASSED,
        "    steps:\n",
        "    steps:\n      - uses: ./.github/actions/upload\n",
    );
    red(
        &w.problems(),
        "the job `passed` of pipeline.yml runs the local action `./.github/actions/upload`",
    );
}

#[test]
fn a_download_selector_is_read_as_the_action_reads_it() {
    // Catches: an attest or release download whose `name:` holds a wildcard (the action looks
    // one name up, so `name: assets-linux-*` reads nothing), or whose selector differs in case
    // (the action's pattern match is case-sensitive), taken for a reader of the seed files.
    for (from, to, want) in [
        (
            "pattern: assets-linux-*\n",
            "name: assets-linux-*\n",
            "the attest job `attest-linux` does not download `assets-linux-x86_64`",
        ),
        (
            "pattern: assets-linux-*\n",
            "pattern: ASSETS-linux-*\n",
            "the attest job `attest-linux` does not download `assets-linux-x86_64`",
        ),
    ] {
        red(&spoil_artifacts("attest-linux", from, to), want);
    }
    red(
        &spoil_artifacts(
            "attest-darwin-arm64",
            "pattern: assets-darwin-arm64\n",
            "name: Assets-darwin-arm64\n",
        ),
        "the attest job `attest-darwin-arm64` does not download `assets-darwin-arm64`",
    );
    for to in ["name: assets-*\n", "pattern: Assets-*\n"] {
        let mut w = real();
        w.release = in_job(&w.release, RELEASE_VERIFY, "pattern: assets-*\n", to);
        red(
            &w.problems(),
            "the release's `verify` does not download `assets-linux-x86_64`",
        );
    }
    // An exact `name:` is a reader.
    let found = spoil_artifacts(
        "attest-darwin-arm64",
        "pattern: assets-darwin-arm64\n",
        "name: assets-darwin-arm64\n",
    );
    assert!(found.is_empty(), "{found:#?}");
}

#[test]
fn a_seed_upload_of_another_name_case_or_directory_is_refused() {
    // Catches: a seed upload renamed only in case (`Assets-<job>`, which the attest and
    // release selectors, case-sensitive, never read), pointed at a directory other than the
    // packaged release files, or allowed to upload nothing.
    for job in SEED_JOBS {
        let name = format!("name: assets-{job}\n");
        red(
            &spoil_artifacts(job, &name, &format!("name: Assets-{job}\n")),
            &format!("the seed job `{job}` does not upload its files as `assets-{job}` (exactly)"),
        );
        for to in [
            "          path: build/\n",
            "          path: target/release/\n",
        ] {
            red(
                &spoil_artifacts(job, "          path: dist/\n", to),
                &format!("the seed job `{job}` uploads `assets-{job}` from Some("),
            );
        }
        red(
            &spoil_artifacts(
                job,
                "          if-no-files-found: error\n",
                "          if-no-files-found: warn\n",
            ),
            "without `if-no-files-found: error`",
        );
    }
}

#[test]
fn a_seed_upload_by_a_sub_action_is_refused() {
    // Catches: a seed upload replaced by `actions/upload-artifact/merge` with the same inputs,
    // which re-uploads artifacts that already exist, not the job's `dist`.
    let uses =
        "- uses: actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a # v7.0.1\n";
    for job in SEED_JOBS {
        red(
            &spoil_artifacts(
                job,
                uses,
                &uses.replace("upload-artifact@", "upload-artifact/merge@"),
            ),
            &format!("the seed job `{job}` does not upload its files as `assets-{job}` (exactly)"),
        );
    }
}

#[test]
fn a_download_that_does_not_put_the_seed_files_in_dist_is_refused() {
    // Catches: an attest or release download of the seed files sent to another directory,
    // unmerged (each artifact in a directory of its own) or left as zips, so the checks that
    // read `dist` no longer read the seed files.
    let merged = "          path: dist\n          merge-multiple: true\n";
    for to in [
        "          path: other\n          merge-multiple: true\n",
        "          path: dist\n          merge-multiple: false\n",
        "          path: dist\n",
        "          path: dist\n          merge-multiple: true\n          skip-decompress: true\n",
    ] {
        for (job, want) in [
            (
                "attest-linux",
                "the attest job `attest-linux` does not download `assets-linux-x86_64`",
            ),
            (
                "attest-darwin-arm64",
                "the attest job `attest-darwin-arm64` does not download `assets-darwin-arm64`",
            ),
        ] {
            let mut w = real();
            let (all, start, end) = job_span(&w.artifacts, job);
            let region = all[start..end].concat();
            let first = region
                .find(merged)
                .unwrap_or_else(|| panic!("{job} merges into dist"));
            let spoiled = format!(
                "{}{to}{}",
                &region[..first],
                &region[first + merged.len()..]
            );
            w.artifacts = [all[..start].concat(), spoiled, all[end..].concat()].concat();
            red(&w.problems(), want);
        }
        let mut w = real();
        let from = format!("          pattern: assets-*\n{merged}");
        w.release = in_job(
            &w.release,
            RELEASE_VERIFY,
            &from,
            &format!("          pattern: assets-*\n{to}"),
        );
        red(
            &w.problems(),
            "the release's `verify` does not download `assets-darwin-arm64`",
        );
    }
    // `skip-decompress: false` is the default, and still extracts.
    let found = spoil_artifacts(
        "attest-darwin-arm64",
        "          pattern: assets-darwin-arm64\n",
        "          pattern: assets-darwin-arm64\n          skip-decompress: false\n",
    );
    assert!(found.is_empty(), "{found:#?}");
}

#[test]
fn a_download_from_another_repository_is_refused() {
    // Catches: an attest or release download of the seed files pointed at another
    // repository (`repository:`), whose artifacts are not this repository's seed build.
    let pipeline_run = "          run-id: ${{ github.event.workflow_run.id }}\n";
    let from = format!(
        "          pattern: assets-*\n          path: dist\n          merge-multiple: true\n{pipeline_run}"
    );
    for repository in ["someone/kbf-farm", "komira-ai/kbf-builds"] {
        let mut w = real();
        w.release = in_job(
            &w.release,
            RELEASE_VERIFY,
            &from,
            &format!("{from}          repository: {repository}\n"),
        );
        red(
            &w.problems(),
            "the release's `verify` does not download `assets-linux-x86_64`",
        );
        red(
            &spoil_artifacts(
                "attest-linux",
                "          pattern: assets-linux-*\n",
                &format!("          pattern: assets-linux-*\n          repository: {repository}\n"),
            ),
            "the attest job `attest-linux` does not download `assets-linux-x86_64`",
        );
    }
    // This repository, named, is the same download.
    let mut w = real();
    w.release = in_job(
        &w.release,
        RELEASE_VERIFY,
        &from,
        &format!("{from}          repository: ${{{{ github.repository }}}}\n"),
    );
    let found = w.problems();
    assert!(found.is_empty(), "{found:#?}");
}
