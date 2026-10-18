//! The rules [`gaps`] reads the workflows by, on workflows built to meet them and then
//! broken one way at a time.

use super::*;

const ATTEST_PIN: &str = "4d101475d8b20a2381f78447822ac1eab6504dd8 # v4.2.2";
const UPLOAD_PIN: &str = "043fb46d1a93c77aae656e7c1c64a875d1fc6a0a # v7.0.1";
const DOWNLOAD_PIN: &str = "3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c # v8.0.1";

/// What the fixture's build job packages: two Linux architectures (one attest job) and
/// macOS (its own).
const PACKAGES: [(&str, &str); 4] = [
    ("kbf", "linux-x86_64"),
    ("kbf-server", "linux-x86_64"),
    ("kbf", "linux-arm64"),
    ("kbf", "darwin-arm64"),
];

/// The artifacts `release locate` lists for the fixture.
const LISTED: [&str; 3] = ["sha256sums", "bundles-linux", "bundles-darwin-arm64"];

/// The step id of the provenance step of a tarball.
fn id(name: &str, platform: &str) -> String {
    format!("provenance-{name}-{platform}")
}

/// The provenance step of a tarball, as the workflow has it.
fn provenance(name: &str, platform: &str) -> String {
    format!(
        "      - id: {}\n        uses: actions/attest-build-provenance@{ATTEST_PIN}\n        with:\n          subject-path: dist/{}.tar.gz\n",
        id(name, platform),
        attested_assets::stem(name, platform)
    )
}

/// The step after it, which names its bundle.
fn naming(name: &str, platform: &str) -> String {
    format!(
        "      - name: Name the bundle of {name} for {platform}\n        env:\n          BUNDLE: ${{{{ steps.{}.outputs.bundle-path }}}}\n        run: cp \"$BUNDLE\" \"dist/{name}-${{GITHUB_SHA}}-{platform}.tar.gz.sigstore.json\"\n",
        id(name, platform)
    )
}

/// The upload of the bundles of the group `g`.
fn upload(g: &str) -> String {
    format!(
        "      - uses: actions/upload-artifact@{UPLOAD_PIN}\n        with:\n          name: bundles-{g}\n          path: dist/*.sigstore.json\n          retention-days: 90\n          if-no-files-found: error\n"
    )
}

/// The step of the verify job of the group `g` that downloads its bundles.
fn download(g: &str) -> String {
    format!(
        "      - uses: actions/download-artifact@{DOWNLOAD_PIN}\n        with:\n          name: bundles-{g}\n          path: dist\n"
    )
}

/// artifacts.yml as the rules want it.
fn good_artifacts() -> String {
    let mut w = String::from(
        "on: push\npermissions: {}\njobs:\n  build:\n    runs-on: ubuntu-24.04\n    steps:\n",
    );
    for (name, platform) in PACKAGES {
        w.push_str(&format!(
            "      - name: Package {name}\n        run: cargo xtask package --binary \"target/$TARGET/release/{name}\" --sbom crates/{name}/{name}_bin.cdx.json --platform {platform} --commit \"$GITHUB_SHA\" --out dist\n"
        ));
    }
    for g in ["linux", "darwin-arm64"] {
        w.push_str(&format!(
            "  attest-{g}:\n    runs-on: ubuntu-24.04\n    steps:\n      - uses: actions/attest-build-provenance@{ATTEST_PIN}\n        with:\n          subject-checksums: dist/SHA256SUMS\n"
        ));
        for (name, platform) in PACKAGES.iter().filter(|(_, p)| group(p) == g) {
            w.push_str(&provenance(name, platform));
            w.push_str(&naming(name, platform));
        }
        w.push_str(&upload(g));
        w.push_str(&format!(
            "  verify-{g}:\n    runs-on: ubuntu-24.04\n    steps:\n{}",
            download(g)
        ));
    }
    w
}

/// The release workflow's verify job's download of the bundles.
const RELEASE_DOWNLOAD: &str = "      - uses: actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c # v8.0.1\n        with:\n          pattern: bundles-*\n          path: dist\n          merge-multiple: true\n";

/// Its fetch of the trusted root.
const RELEASE_FETCH: &str = "      - name: Fetch the trusted root\n        run: gh attestation trusted-root > \"$RUNNER_TEMP/trusted_root.jsonl\"\n";

/// The release workflow's verify job as the rules want it.
const GOOD_RELEASE: &str = "on: push\npermissions: {}\njobs:\n  verify:\n    runs-on: ubuntu-24.04\n    steps:\n      - uses: actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c # v8.0.1\n        with:\n          pattern: bundles-*\n          path: dist\n          merge-multiple: true\n      - name: Fetch the trusted root\n        run: gh attestation trusted-root > \"$RUNNER_TEMP/trusted_root.jsonl\"\n      - name: Verify\n        run: cargo xtask verify-assets attestations dist --repo \"$GITHUB_REPOSITORY\" --workflow-ref \"$GITHUB_REPOSITORY/.github/workflows/artifacts.yml\" --commit \"$COMMIT\" --trusted-root \"$RUNNER_TEMP/trusted_root.jsonl\"\n";

fn found(artifacts: &str, release: &str, listed: &[&str]) -> Vec<String> {
    gaps(artifacts, release, listed).expect("readable")
}

/// `good_artifacts()` with `from` replaced by `to`, once; `from` must be there.
fn broken(from: &str, to: &str) -> String {
    let w = good_artifacts();
    assert!(w.contains(from), "{from:?} is not in the planted workflow");
    w.replacen(from, to, 1)
}

/// Planted defects checked one after the other, every one that goes unnamed reported
/// together at the end, so a lint that stops naming one shows which rules it broke
/// rather than the first.
#[derive(Default)]
struct Cases(Vec<String>);

impl Cases {
    /// Records the case `what` unless the problems of `artifacts` (with the good
    /// release workflow) hold one containing `wanted` and, when `alone`, no other.
    fn artifacts(&mut self, what: &str, artifacts: &str, wanted: &str, alone: bool) {
        let got = found(artifacts, GOOD_RELEASE, &LISTED);
        self.check(what, got, wanted, alone);
    }

    /// The same for the release workflow `release` (with the good artifacts workflow).
    fn release(&mut self, what: &str, release: &str, wanted: &str) {
        let got = found(&good_artifacts(), release, &LISTED);
        self.check(what, got, wanted, true);
    }

    fn check(&mut self, what: &str, got: Vec<String>, wanted: &str, alone: bool) {
        if !got.iter().any(|p| p.contains(wanted)) {
            self.0.push(format!(
                "{what}: wanted a problem containing {wanted:?}, got {got:#?}"
            ));
        } else if alone && got.len() != 1 {
            self.0
                .push(format!("{what}: wanted {wanted:?} alone, got {got:#?}"));
        }
    }

    /// Fails with every recorded case.
    fn done(self) {
        assert!(
            self.0.is_empty(),
            "{} case(s) not named:\n{}",
            self.0.len(),
            self.0.join("\n")
        );
    }
}

#[test]
fn workflows_meeting_every_rule_pass() {
    // Catches: a rule misreading a correct workflow (ids, expressions in `env:`, the
    // quoted copy command, two groups, a glob in the upload), so every real change is
    // refused, and every planted defect below would go red for the wrong reason.
    assert_eq!(
        found(&good_artifacts(), GOOD_RELEASE, &LISTED),
        Vec::<String>::new()
    );
}

#[test]
fn a_tarball_without_its_own_provenance_step_is_named() {
    // Catches: the defect the lint exists for, a binary added to a build job with
    // no attestation of its own (it would have no bundle), for each tarball of the
    // set, not only the first or the last.
    let mut cases = Cases::default();
    for (name, platform) in PACKAGES {
        let w = good_artifacts();
        let step = provenance(name, platform);
        assert!(w.contains(&step));
        let planted = w
            .replacen(&step, "", 1)
            .replacen(&naming(name, platform), "", 1);
        cases.artifacts(
            &format!("{name} {platform}"),
            &planted,
            &format!(
                "{name} for {platform} is packaged, but `attest-{}` has no actions/attest-build-provenance step with subject-path dist/{}.tar.gz",
                group(platform),
                attested_assets::stem(name, platform)
            ),
            true,
        );
    }
    cases.done();
}

#[test]
fn a_provenance_step_shared_by_several_tarballs_is_not_an_own_subject() {
    // Catches: a glob or a list in `subject-path`, which signs one attestation over
    // several tarballs (one bundle for all, so a bundle that verifies another's
    // tarball), and the checksums step standing in for the per-tarball one.
    let mut cases = Cases::default();
    for shared in [
        "dist/*.tar.gz",
        "dist/kbf-a.tar.gz\n            dist/kbf-b.tar.gz",
    ] {
        let (name, platform) = PACKAGES[1];
        let own = format!(
            "subject-path: dist/{}.tar.gz",
            attested_assets::stem(name, platform)
        );
        let planted = broken(&own, &format!("subject-path: {shared}"));
        cases.artifacts(
            &format!("{shared:?}, the tarball has none of its own"),
            &planted,
            &format!(
                "{name} for {platform} is packaged, but `attest-linux` has no actions/attest-build-provenance step with subject-path"
            ),
            false,
        );
        cases.artifacts(
            &format!("{shared:?}, the shared step is named"),
            &planted,
            "a tarball no job of its platform group packages",
            false,
        );
    }
    cases.done();
}

#[test]
fn a_provenance_step_for_a_tarball_the_group_does_not_package_is_named() {
    // Catches: a step left behind for a tarball no longer built (the attest job fails
    // on main: the file is not there), and one in the wrong platform group's job (a
    // macOS tarball attested in the Linux job, which never downloads it).
    let mut cases = Cases::default();
    let stale = format!(
        "{}{}",
        provenance("kbf-old", "linux-x86_64"),
        naming("kbf-old", "linux-x86_64")
    );
    cases.artifacts(
        "a tarball no longer built",
        &broken(&upload("linux"), &format!("{stale}{}", upload("linux"))),
        "`attest-linux` attests dist/kbf-old-${{ github.sha }}-linux-x86_64.tar.gz with a provenance step of its own",
        true,
    );
    let darwin = format!(
        "{}{}",
        provenance("kbf", "darwin-arm64"),
        naming("kbf", "darwin-arm64")
    );
    cases.artifacts(
        "a macOS tarball in the Linux job",
        &broken(&upload("linux"), &format!("{darwin}{}", upload("linux"))),
        "`attest-linux` attests dist/kbf-${{ github.sha }}-darwin-arm64.tar.gz with a provenance step of its own",
        true,
    );
    cases.done();
}

#[test]
fn a_skippable_provenance_step_or_one_without_an_id_is_named() {
    // Catches: a provenance step under `if:` or `continue-on-error:` (the job passes
    // without the attestation), and one with no `id:` (its bundle cannot be reached).
    let mut cases = Cases::default();
    let (name, platform) = PACKAGES[0];
    let step = provenance(name, platform);
    for extra in [
        "        if: always()\n",
        "        continue-on-error: true\n",
    ] {
        cases.artifacts(
            extra.trim(),
            &broken(&step, &format!("{step}{extra}")),
            &format!(
                "the provenance step for {name} for {platform} in `attest-linux` is under `if:`"
            ),
            true,
        );
    }
    let no_id = step.replacen(
        &format!("      - id: {}\n        uses:", id(name, platform)),
        "      - uses:",
        1,
    );
    assert_ne!(no_id, step);
    cases.artifacts(
        "no id",
        &broken(&step, &no_id),
        &format!("the provenance step for {name} for {platform} in `attest-linux` has no `id:`"),
        true,
    );
    cases.done();
}

#[test]
fn a_step_that_does_not_name_the_bundle_after_its_tarball_is_named() {
    // Catches: the bundle never copied out of the action's temporary directory (the
    // upload finds nothing, or another tarball's bundle), copied to another tarball's
    // name (a bundle that verifies nothing it is next to), read from another step's
    // output, skippable, or not the step right after its provenance step.
    let mut cases = Cases::default();
    let (name, platform) = PACKAGES[1];
    let step = naming(name, platform);
    let (other, other_platform) = PACKAGES[0];
    let wanted = format!(
        "the step after the provenance step for {name} for {platform} in `attest-linux` does not name its bundle"
    );
    let plants = [
        ("missing", String::new()),
        (
            "another tarball's name",
            step.replace(
                &format!("dist/{name}-${{GITHUB_SHA}}-{platform}"),
                &format!("dist/{other}-${{GITHUB_SHA}}-{other_platform}"),
            ),
        ),
        (
            "another step's output",
            step.replace(
                &format!("steps.{}.", id(name, platform)),
                &format!("steps.{}.", id(other, other_platform)),
            ),
        ),
        (
            "copied from a literal",
            step.replace("\"$BUNDLE\"", "attestation.json"),
        ),
        ("no env", step.replace("        env:\n", "        x:\n")),
        (
            "conditional",
            step.replace("        env:\n", "        if: always()\n        env:\n"),
        ),
        (
            "a second command",
            step.replace("sigstore.json\"\n", "sigstore.json\" && true\n"),
        ),
    ];
    for (what, replacement) in plants {
        assert_ne!(replacement, step, "{what}: the plant changed nothing");
        cases.artifacts(what, &broken(&step, &replacement), &wanted, true);
    }
    // Moved: the step is there, but not right after its provenance step. It is then a
    // `run:` step that follows no provenance step too, which is named separately.
    let w = good_artifacts();
    let moved = w.replacen(&step, "", 1).replacen(
        &upload("linux"),
        &format!("{step}{}", upload("linux")),
        1,
    );
    assert_ne!(moved, w);
    cases.artifacts("moved", &moved, &wanted, false);
    cases.artifacts(
        "moved, the step is a stray one",
        &moved,
        "`attest-linux` has a `run:` step (`cp \"$BUNDLE\"",
        false,
    );
    cases.done();
}

#[test]
fn a_run_step_in_a_signing_job_that_names_no_bundle_is_named() {
    // Catches: the job that holds `id-token: write` growing a shell step of its own (a
    // script, a build, a download piped to a shell) anywhere in it: before the first
    // attestation, between two tarballs' steps, after the upload, in the second
    // platform group's job, skippable, or one with a name and several lines. The comment
    // on the job says it signs and does nothing else; every such step passes every
    // other rule here, the real naming steps among them.
    let mut cases = Cases::default();
    let extra = "      - run: echo extra\n";
    let wanted = "`attest-linux` has a `run:` step (`echo extra`) that is not the naming of a bundle right after a provenance step";
    let head = "  attest-linux:\n    runs-on: ubuntu-24.04\n    steps:\n";
    let (name, platform) = PACKAGES[1];
    let step = provenance(name, platform);
    let up = upload("linux");
    let plants = [
        ("first", broken(head, &format!("{head}{extra}"))),
        (
            "right after the provenance step over the sums, which is not a tarball's",
            broken(
                "subject-checksums: dist/SHA256SUMS\n",
                &format!("subject-checksums: dist/SHA256SUMS\n{extra}"),
            ),
        ),
        ("between", broken(&step, &format!("{extra}{step}"))),
        ("after the upload", broken(&up, &format!("{up}{extra}"))),
        (
            "skippable",
            broken(
                &up,
                &format!("{up}      - if: always()\n        run: echo extra\n"),
            ),
        ),
    ];
    for (what, planted) in plants {
        cases.artifacts(what, &planted, wanted, true);
    }
    cases.artifacts(
        "named and several lines",
        &broken(
            &up,
            &format!(
                "{up}      - name: Build\n        run: |\n          cargo build --release\n          ./deploy\n"
            ),
        ),
        "`attest-linux` has a `run:` step (`cargo build --release`) that is not the naming of a bundle",
        true,
    );
    let darwin = upload("darwin-arm64");
    cases.artifacts(
        "the other platform group's job",
        &broken(&darwin, &format!("{darwin}{extra}")),
        "`attest-darwin-arm64` has a `run:` step (`echo extra`) that is not the naming of a bundle",
        true,
    );
    cases.done();
}

#[test]
fn the_bundles_must_leave_the_job_in_a_step_that_cannot_be_skipped() {
    // Catches: no upload (the bundles die with the runner), an upload of another name
    // (the verify job and `release locate` look for `bundles-<group>`), of another path
    // (the glob that matches no bundle), one that passes on no file, one that can be
    // skipped, and one before the bundles are named.
    let mut cases = Cases::default();
    let up = upload("linux");
    let plants = [
        (
            "missing",
            String::new(),
            "`attest-linux` has no actions/upload-artifact step for `bundles-linux`",
        ),
        (
            "another name",
            up.replace("name: bundles-linux", "name: bundles"),
            "`attest-linux` has no actions/upload-artifact step for `bundles-linux`",
        ),
        (
            "another path",
            up.replace("dist/*.sigstore.json", "dist/"),
            "the upload of `bundles-linux` in `attest-linux` has path \"dist/\"",
        ),
        (
            "passes on no file",
            up.replace("          if-no-files-found: error\n", ""),
            "the upload of `bundles-linux` in `attest-linux` does not fail on no file",
        ),
        (
            "conditional",
            up.replace("        with:\n", "        if: always()\n        with:\n"),
            "the upload of `bundles-linux` in `attest-linux` is under `if:`",
        ),
    ];
    for (what, replacement, wanted) in plants {
        assert_ne!(replacement, up, "{what}: the plant changed nothing");
        cases.artifacts(what, &broken(&up, &replacement), wanted, true);
    }
    // Before the last bundle is named: the upload moved up, past the first tarball's
    // steps only.
    let first = format!(
        "{}{}",
        provenance(PACKAGES[0].0, PACKAGES[0].1),
        naming(PACKAGES[0].0, PACKAGES[0].1)
    );
    let early = good_artifacts()
        .replacen(&up, "", 1)
        .replacen(&first, &format!("{first}{up}"), 1);
    cases.artifacts(
        "before the last bundle is named",
        &early,
        "the upload of `bundles-linux` in `attest-linux` comes before the last bundle is named",
        true,
    );
    cases.done();
}

#[test]
fn the_verify_job_must_download_the_bundles_into_dist() {
    // Catches: a verify job that verifies the bundles of nothing (no download), the
    // wrong artifact, or a directory other than the one the files are in.
    let mut cases = Cases::default();
    let down = download("darwin-arm64");
    let wanted = "`verify-darwin-arm64` does not download `bundles-darwin-arm64` into `dist`";
    for (what, replacement) in [
        // (Another step stays, so the job still has a steps list.)
        ("missing", "      - run: echo hi\n".to_owned()),
        (
            "another artifact",
            down.replace("bundles-darwin-arm64", "bundles-linux"),
        ),
        (
            "another directory",
            down.replace("path: dist", "path: bundles"),
        ),
    ] {
        assert_ne!(replacement, down, "{what}");
        cases.artifacts(what, &broken(&down, &replacement), wanted, true);
    }
    cases.done();
}

#[test]
fn the_release_must_download_the_bundles_and_verify_them_offline_for_its_commit() {
    // Catches: a release that publishes what it never checked bundles of (no download of
    // `bundles-*`), one that verifies for no commit or the wrong one (`$GITHUB_SHA` of
    // the tag push is the tag's commit, but of a dry run it is the branch's), one that
    // does not fetch the trusted root or reaches for it after the verification, and one
    // that points the verification at another file.
    let mut cases = Cases::default();
    assert!(GOOD_RELEASE.contains(RELEASE_DOWNLOAD) && GOOD_RELEASE.contains(RELEASE_FETCH));
    let plants = [
        (
            "no download",
            GOOD_RELEASE.replacen(RELEASE_DOWNLOAD, "", 1),
            "does not download `bundles-*` into `dist`",
        ),
        (
            "not merged",
            GOOD_RELEASE.replacen("          merge-multiple: true\n", "", 1),
            "does not download `bundles-*` into `dist`",
        ),
        (
            "no trusted root",
            GOOD_RELEASE.replacen(RELEASE_FETCH, "", 1),
            "does not fetch the trusted root",
        ),
        (
            "no commit",
            GOOD_RELEASE.replacen(" --commit \"$COMMIT\"", "", 1),
            "without `--commit \"$COMMIT\"`",
        ),
        (
            "the branch's commit",
            GOOD_RELEASE.replacen("--commit \"$COMMIT\"", "--commit \"$GITHUB_SHA\"", 1),
            "without `--commit \"$COMMIT\"`",
        ),
        (
            "online",
            GOOD_RELEASE.replacen(" --trusted-root \"$RUNNER_TEMP/trusted_root.jsonl\"", "", 1),
            "does not verify offline",
        ),
        (
            "another root",
            GOOD_RELEASE.replacen(
                "--trusted-root \"$RUNNER_TEMP/trusted_root.jsonl\"",
                "--trusted-root other.jsonl",
                1,
            ),
            "does not verify offline",
        ),
        (
            "root fetched after",
            GOOD_RELEASE.replacen(RELEASE_FETCH, "", 1) + RELEASE_FETCH,
            "does not verify offline",
        ),
    ];
    for (what, release, wanted) in plants {
        assert_ne!(release, GOOD_RELEASE, "{what}: the plant changed nothing");
        cases.release(what, &release, wanted);
    }
    cases.done();
}

#[test]
fn release_locate_must_require_exactly_the_bundles_the_attest_jobs_upload() {
    // Catches: a `bundles-*` artifact the jobs upload but `release locate` does not
    // require (a run that lost it is released), and one it requires that nothing uploads
    // (every release refused).
    let got = found(
        &good_artifacts(),
        GOOD_RELEASE,
        &["sha256sums", "bundles-linux"],
    );
    assert_eq!(
        got,
        vec!["`xtask::release::ARTIFACTS` does not list `bundles-darwin-arm64`: `release locate` would take a run that lost its bundles".to_owned()]
    );
    let got = found(
        &good_artifacts(),
        GOOD_RELEASE,
        &["bundles-linux", "bundles-darwin-arm64", "bundles-windows"],
    );
    assert_eq!(
        got,
        vec!["`xtask::release::ARTIFACTS` lists `bundles-windows`, which no attest job uploads: `release locate` would refuse every run".to_owned()]
    );
}

#[test]
fn a_workflow_packaging_nothing_is_refused() {
    // Catches: the lint passing vacuously when the package lines move or change their
    // spelling, so it reads none.
    let w = good_artifacts().replace("cargo xtask package", "# cargo");
    assert!(gaps(&w, GOOD_RELEASE, &LISTED).is_err());
}
