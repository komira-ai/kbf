//! `cargo xtask package`, `sums assemble` and `verify-assets` run as the artifacts
//! workflow runs them, with `xtask-fake` as `gh` and its file `attested` as the store
//! of attestations.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use kbf_testkit::dir::TestDir;
use sha2::Digest;

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
const FAKE: &str = env!("CARGO_BIN_EXE_xtask-fake");
const COMMIT: &str = "c0ffee0123456789c0ffee0123456789c0ffee01";
const REPO: &str = "komira-ai/kbf";
const WORKFLOW: &str = "komira-ai/kbf/.github/workflows/artifacts.yml";
const SBOM_PREDICATE: &str = "--predicate-type https://cyclonedx.org/bom";
const ISSUER: &str = "https://token.actions.githubusercontent.com";
const PROVENANCE: &str = "https://slsa.dev/provenance/v1";
/// The proxy a verification with a trusted root runs under: nothing listens on it.
const DEAD_PROXY: &str = "http://127.0.0.1:9";
/// A commit the files were not built from.
const OTHER: &str = "badbad0123456789badbad0123456789badbad01";

/// An empty directory of its own for one test.
fn scratch(name: &str) -> TestDir {
    TestDir::under(
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("xtask-assets"),
        name,
    )
}

/// The exit code, stdout and stderr of `cargo xtask <args>`.
fn xtask(args: &[&str], tmp: &Path, gh_defect: Option<&str>) -> (Option<i32>, String, String) {
    let mut cmd = Command::new(XTASK);
    // A runner whose environment lets every host past a proxy: the dead proxy of an
    // offline verification must not be bypassed by it.
    cmd.args(args)
        .env("TMPDIR", tmp)
        .env("NO_PROXY", "*")
        .env("no_proxy", "*");
    match gh_defect {
        Some(d) => cmd.env("XTASK_FAKE_DEFECT", d),
        None => cmd.env_remove("XTASK_FAKE_DEFECT"),
    };
    let out: Output = cmd.output().expect("run xtask");
    let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    (out.status.code(), text(&out.stdout), text(&out.stderr))
}

fn s(p: &Path) -> &str {
    p.to_str().expect("utf-8 path")
}

/// The flags a deployment verifies an asset with (docs/artifacts.md), signed by
/// `signer`, from the branch `source_ref`.
fn flags_from(signer: &str, source_ref: &str) -> String {
    format!(
        "--repo {REPO} --signer-workflow {signer} --source-ref {source_ref} \
         --deny-self-hosted-runners"
    )
}

/// [`flags_from`] `refs/heads/main`.
fn flags(signer: &str) -> String {
    flags_from(signer, "refs/heads/main")
}

/// The flags a host's installer verifies a tarball with against its bundle, signed by
/// `artifacts.yml` from the branch `source_ref` for `commit` (the `--bundle` and
/// `--custom-trusted-root` words name files and are not part of it).
fn bundle_flags(source_ref: &str, commit: &str) -> String {
    format!(
        "--repo {REPO} --signer-workflow {WORKFLOW} --cert-oidc-issuer {ISSUER} \
         --source-ref {source_ref} --source-digest {commit} --predicate-type {PROVENANCE} \
         --deny-self-hosted-runners"
    )
}

/// The bundle file of the tarball with the digest `sha`, as the attest job leaves it: the
/// shape of a Sigstore bundle whose payload is the in-toto statement of one build
/// provenance attestation with that one subject, and which the fake gh reads as the one
/// attestation `<sha> <flags>`.
fn bundle_json(sha: &str, flags: &str) -> String {
    let statement = serde_json::json!({
        "_type": "https://in-toto.io/Statement/v1",
        "subject": [{ "name": "asset", "digest": { "sha256": sha } }],
        "predicateType": PROVENANCE,
        "predicate": {},
    });
    let bundle = serde_json::json!({
        "mediaType": "application/vnd.dev.sigstore.bundle.v0.3+json",
        "verificationMaterial": {},
        "dsseEnvelope": {
            "payloadType": "application/vnd.in-toto+json",
            "payload": STANDARD.encode(statement.to_string()),
            "signatures": [],
        },
        "fakeAttestations": [format!("{sha} {flags}")],
    });
    format!("{bundle}\n")
}

/// The branch the development line was built and attested on until main became the only
/// integration branch (docs/artifacts.md). Kept: attestations it signed still exist and
/// must verify as that branch's, never as main's.
const DEV_REF: &str = "refs/heads/feature/arch-v2";

/// What the release path leaves: `<root>/dist` packaged and summed as the workflow
/// does, `<root>/gh/gh` the fake gh, `<root>/tmp` the temporary directory.
struct Release {
    dist: PathBuf,
    gh: PathBuf,
    tmp: PathBuf,
    /// The directory all of it is in; last, so it is removed after the rest.
    _root: TestDir,
}

impl Release {
    /// Packages kbf-daemon for every platform and kbf-server for Linux, then
    /// assembles SHA256SUMS, each through the CLI as the workflow runs it.
    fn build(name: &str) -> Release {
        Release::build_with(
            name,
            &[
                ("darwin-arm64", &["kbf-daemon"][..]),
                ("linux-x86_64", &["kbf-daemon", "kbf-server"][..]),
                ("linux-arm64", &["kbf-daemon", "kbf-server"][..]),
            ],
        )
    }

    /// [`Release::build`] of the binaries `jobs` name for each platform.
    fn build_with(name: &str, jobs: &[(&str, &[&str])]) -> Release {
        let root = scratch(name);
        let bin = root.join("bin");
        std::fs::create_dir(&bin).expect("bin");
        let tmp = root.join("tmp");
        std::fs::create_dir(&tmp).expect("tmp");
        let dist = root.join("dist");
        let mut files = 0;
        for (platform, names) in jobs.iter().copied() {
            files += 2 * names.len();
            for name in names {
                let binary = bin.join(name);
                std::fs::write(&binary, format!("{name} for {platform}")).expect("binary");
                // Distinct bytes: an attestation covers a digest, not a name.
                let sbom = root.join(format!("{name}-{platform}.cdx.json"));
                std::fs::write(&sbom, format!("{{\"for\":\"{name} {platform}\"}}")).expect("sbom");
                let (code, out, err) = xtask(
                    &[
                        "package",
                        "--binary",
                        s(&binary),
                        "--sbom",
                        s(&sbom),
                        "--platform",
                        platform,
                        "--commit",
                        COMMIT,
                        "--out",
                        s(&dist),
                    ],
                    &tmp,
                    None,
                );
                assert_eq!(code, Some(0), "{err}");
                let stem = format!("{name}-{COMMIT}-{platform}");
                assert!(out.starts_with(&format!("packaged {stem}:\n")), "{out}");
            }
        }
        let summary = root.join("summary.md");
        let (code, out, err) = xtask(
            &["sums", "assemble", s(&dist), "--summary", s(&summary)],
            &tmp,
            None,
        );
        assert_eq!(code, Some(0), "{err}");
        assert_eq!(out.lines().count(), files, "{out}");
        let gh = root.join("gh").join("gh");
        std::fs::create_dir(root.join("gh")).expect("gh dir");
        std::fs::hard_link(FAKE, &gh).expect("link the fake as gh");
        Release {
            dist,
            gh,
            tmp,
            _root: root,
        }
    }

    /// The SHA256SUMS lines: (digest, name).
    fn sums(&self) -> Vec<(String, String)> {
        std::fs::read_to_string(self.dist.join("SHA256SUMS"))
            .expect("SHA256SUMS")
            .lines()
            .map(|l| {
                let (sha, name) = l.split_once("  ").expect("line");
                (sha.to_owned(), name.to_owned())
            })
            .collect()
    }

    /// Records in the fake's store the attestations the attest job signs: provenance
    /// for every file, an SBOM attestation for every tarball, all by `artifacts.yml`.
    /// `skip` leaves out the one for that (name, predicate).
    fn attest(&self, skip: Option<(&str, bool)>) {
        self.attest_from(skip, "refs/heads/main");
    }

    /// The files with a bundle: the tarballs, the `.deb` packages and the image tarballs.
    fn bundled(&self) -> Vec<(String, String)> {
        self.sums()
            .into_iter()
            .filter(|(_, name)| {
                name.ends_with(".tar.gz")
                    || name.ends_with(".deb")
                    || name.ends_with(".pkg")
                    || name.ends_with(".oci.tar")
            })
            .collect()
    }

    /// Whether the sums list the SBOM of the image `name` (`<image stem>.cdx.json`): the one
    /// image an SBOM attestation covers.
    fn has_image_sbom(&self, name: &str) -> bool {
        name.strip_suffix(".oci.tar").is_some_and(|stem| {
            let sbom = format!("{stem}.cdx.json");
            self.sums().iter().any(|(_, n)| *n == sbom)
        })
    }

    /// Adds a deb of `platform` as `pkg-kbf-asset` leaves it: the file in
    /// `dist` (distinct bytes: an attestation covers a digest), listed in `SHA256SUMS`
    /// (sorted by name, as `sums assemble` leaves it).
    fn add_deb(&self, platform: &str) -> String {
        self.add_package(platform, "deb")
    }

    /// Adds the `kbf-installer` package of `platform` with the extension `ext` (`deb`, or
    /// `pkg` for the Mac's), as [`Release::add_deb`] does.
    fn add_package(&self, platform: &str, ext: &str) -> String {
        let name = format!("kbf-installer-{COMMIT}-{platform}.{ext}");
        let bytes = format!("the kbf-installer package for {platform}");
        self.add_listed(&name, &bytes);
        name
    }

    /// Adds the version facts of `kbf` for `platform` as `release version-facts` leaves
    /// them: a file in `dist` (distinct bytes), listed in `SHA256SUMS`.
    fn add_facts(&self, platform: &str) -> String {
        let name = format!("kbf-{COMMIT}-{platform}.version.json");
        let bytes = format!("the version facts of kbf for {platform}\n");
        self.add_listed(&name, &bytes);
        name
    }

    /// Adds the file `name` holding `bytes` to `dist` and to `SHA256SUMS`, which stays
    /// sorted by name as `sums assemble` leaves it.
    fn add_listed(&self, name: &str, bytes: &str) {
        std::fs::write(self.dist.join(name), bytes).expect("file");
        let mut lines: Vec<String> = std::fs::read_to_string(self.dist.join("SHA256SUMS"))
            .expect("SHA256SUMS")
            .lines()
            .map(str::to_owned)
            .collect();
        lines.push(format!(
            "{}  {name}",
            hex::encode(sha2::Sha256::digest(bytes))
        ));
        lines.sort_by_key(|l| l.split_once("  ").map(|(_, n)| n.to_owned()));
        std::fs::write(self.dist.join("SHA256SUMS"), lines.join("\n") + "\n").expect("sums");
    }

    /// Writes next to each tarball and package the bundle the attest job leaves: one
    /// attestation of that file, signed from `source_ref` for `commit`.
    fn bundles(&self, source_ref: &str, commit: &str) {
        for (sha, name) in self.bundled() {
            std::fs::write(
                self.dist.join(format!("{name}.sigstore.json")),
                bundle_json(&sha, &bundle_flags(source_ref, commit)),
            )
            .expect("bundle");
        }
    }

    /// [`Release::attest`] for a run on the branch `source_ref`.
    fn attest_from(&self, skip: Option<(&str, bool)>, source_ref: &str) {
        self.bundles(source_ref, COMMIT);
        let mut store = String::new();
        for (sha, name) in self.sums() {
            let mut kinds = vec![false];
            if name.ends_with(".tar.gz") || self.has_image_sbom(&name) {
                kinds.push(true);
            }
            for sbom in kinds {
                if skip == Some((name.as_str(), sbom)) {
                    continue;
                }
                let predicate = if sbom {
                    format!(" {SBOM_PREDICATE}")
                } else {
                    String::new()
                };
                store.push_str(&format!(
                    "{sha} {}{predicate}\n",
                    flags_from(WORKFLOW, source_ref)
                ));
            }
        }
        std::fs::write(self.gh.with_file_name("attested"), store).expect("store");
    }

    fn verify(&self, workflow_ref: &str, defect: Option<&str>) -> (Option<i32>, String, String) {
        self.verify_args(workflow_ref, &[], defect)
    }

    /// [`Release::verify`] with `more` flags.
    fn verify_args(
        &self,
        workflow_ref: &str,
        more: &[&str],
        defect: Option<&str>,
    ) -> (Option<i32>, String, String) {
        self.verify_commit(workflow_ref, COMMIT, more, defect)
    }

    /// [`Release::verify_args`] for the commit `commit`.
    fn verify_commit(
        &self,
        workflow_ref: &str,
        commit: &str,
        more: &[&str],
        defect: Option<&str>,
    ) -> (Option<i32>, String, String) {
        let mut args = vec![
            "verify-assets",
            "attestations",
            s(&self.dist),
            "--repo",
            REPO,
            "--workflow-ref",
            workflow_ref,
            "--commit",
            commit,
            "--gh",
            s(&self.gh),
        ];
        args.extend_from_slice(more);
        xtask(&args, &self.tmp, defect)
    }

    fn gh_calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.gh.with_file_name("gh-args"))
            .expect("gh-args")
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// The `gh attestation verify` calls made with a bundle.
    fn bundle_calls(&self) -> Vec<String> {
        self.gh_calls()
            .into_iter()
            .filter(|c| c.contains(" --bundle "))
            .collect()
    }

    /// The proxy and no-proxy variables of each call made with a bundle, one line each
    /// (`NAME=value`, empty when unset).
    fn bundle_proxies(&self) -> Vec<String> {
        std::fs::read_to_string(self.gh.with_file_name("gh-env"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// The path of the bundle of the tarball `name`.
    fn bundle_of(&self, name: &str) -> PathBuf {
        self.dist.join(format!("{name}.sigstore.json"))
    }
}

fn first_tarball() -> String {
    format!("kbf-daemon-{COMMIT}-darwin-arm64.tar.gz")
}

/// The second tarball in `SHA256SUMS` order, the one the cross-bundle control uses.
fn second_tarball() -> String {
    format!("kbf-daemon-{COMMIT}-linux-arm64.tar.gz")
}

/// `commit` with its last digit changed, as `verify-assets` makes the control commit.
fn control_commit(commit: &str) -> String {
    let (head, last) = commit.split_at(commit.len() - 1);
    format!("{head}{}", if last == "0" { "1" } else { "0" })
}

#[test]
fn the_release_path_packages_sums_and_verifies_every_asset() {
    // Control for the whole path the workflow runs: catches an asset the sums do not
    // cover, a sums check or attestation check that refuses a correct release, a
    // tarball not checked for its SBOM attestation, a verify call with a flag the
    // documented command lacks (the fake verifies only the exact flags), and the
    // flipped copy left in the temporary directory.
    let r = Release::build("release");
    let (code, out, err) = xtask(&["verify-assets", "sums", s(&r.dist)], &r.tmp, None);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(out, "sums: 10 files match; the flipped copy does not\n");
    r.attest(None);
    let (code, out, err) = r.verify(&format!("{WORKFLOW}@refs/heads/main"), None);
    assert_eq!(code, Some(0), "{err}");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 11, "{out}");
    assert_eq!(
        lines[0],
        format!("verified: kbf-daemon-{COMMIT}-darwin-arm64.cdx.json (provenance)")
    );
    assert_eq!(
        lines[1],
        format!("verified: {} (provenance, SBOM, bundle)", first_tarball())
    );
    assert_eq!(
        lines[10],
        "attestations: every asset verifies; another signer workflow, another tarball's bundle, \
         another commit and a flipped byte do not"
    );
    let calls = r.gh_calls();
    // 5 tarballs three times (provenance, SBOM, bundle), 5 SBOMs once, then the five
    // controls: another signer, a flipped byte, another tarball's bundle, another
    // commit, a flipped byte against the bundle.
    assert_eq!(calls.len(), 25, "{calls:#?}");
    let first = r.dist.join(first_tarball());
    assert_eq!(
        calls[1],
        format!("attestation verify {} {}", s(&first), flags(WORKFLOW))
    );
    assert_eq!(
        calls[2],
        format!(
            "attestation verify {} {} {SBOM_PREDICATE}",
            s(&first),
            flags(WORKFLOW)
        )
    );
    // The host's own check of the tarball: its bundle, the installer's flags.
    let bundle = r.bundle_of(&first_tarball());
    assert_eq!(
        calls[3],
        format!(
            "attestation verify {} --bundle {} {}",
            s(&first),
            s(&bundle),
            bundle_flags("refs/heads/main", COMMIT)
        )
    );
    let other = format!("{REPO}/.github/workflows/ci.yml");
    assert_eq!(
        calls[20],
        format!("attestation verify {} {}", s(&first), flags(&other))
    );
    assert!(calls[21].contains("/xtask-verify-"), "{}", calls[21]);
    assert!(calls[21].ends_with(&format!("{} {}", first_tarball(), flags(WORKFLOW))));
    // Another tarball's bundle for the first tarball; the first's for another commit;
    // the first's for a copy with a byte flipped.
    let second = r.bundle_of(&second_tarball());
    assert_eq!(
        calls[22],
        format!(
            "attestation verify {} --bundle {} {}",
            s(&first),
            s(&second),
            bundle_flags("refs/heads/main", COMMIT)
        )
    );
    assert_eq!(
        calls[23],
        format!(
            "attestation verify {} --bundle {} {}",
            s(&first),
            s(&bundle),
            bundle_flags("refs/heads/main", &control_commit(COMMIT))
        )
    );
    assert!(calls[24].contains("/xtask-verify-"), "{}", calls[24]);
    assert!(calls[24].contains(&format!("--bundle {}", s(&bundle))));
    assert_eq!(
        std::fs::read_dir(&r.tmp).expect("tmp").count(),
        0,
        "the flipped copy's directory was left"
    );
    // A workflow named without `@<ref>` is the same workflow.
    let (code, _, err) = r.verify(WORKFLOW, None);
    assert_eq!(code, Some(0), "{err}");
}

#[test]
fn a_push_to_the_development_branch_is_verified_against_that_branch() {
    // Catches: the `--source-ref` of every verification hard-wired to main (the verify
    // job of a push to the development branch would refuse its own attestations); the
    // flag ignored, or applied to the signer controls only; and, as the control that
    // matters for safety, attestations from the development branch accepted when no
    // ref is given or main is: a file built there must never pass a check for main (a
    // release verifies as main).
    let r = Release::build("dev-ref");
    r.attest_from(None, DEV_REF);
    // Control: no `--source-ref` is main, so these attestations do not verify.
    let (code, _, err) = r.verify(&format!("{WORKFLOW}@{DEV_REF}"), None);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("attestation signed by"), "{err}");
    let (code, _, err) = r.verify_args(WORKFLOW, &["--source-ref", "refs/heads/main"], None);
    assert_eq!(code, Some(1), "{err}");
    // Asked for the branch they were signed on, every asset verifies, controls included.
    let (code, out, err) = r.verify_args(WORKFLOW, &["--source-ref", DEV_REF], None);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(out.lines().count(), 11, "{out}");
    let calls = r.gh_calls();
    let first = r.dist.join(first_tarball());
    assert!(
        calls.contains(&format!(
            "attestation verify {} {}",
            s(&first),
            flags_from(WORKFLOW, DEV_REF)
        )),
        "{calls:#?}"
    );
    // Both controls ask for the same branch: another signer, and the flipped copy.
    let other = format!("{REPO}/.github/workflows/ci.yml");
    assert!(
        calls.contains(&format!(
            "attestation verify {} {}",
            s(&first),
            flags_from(&other, DEV_REF)
        )),
        "{calls:#?}"
    );
    assert!(
        calls
            .iter()
            .any(|c| c.contains("/xtask-verify-") && c.ends_with(&flags_from(WORKFLOW, DEV_REF))),
        "{calls:#?}"
    );
    // And main's attestations do not verify as the development branch's.
    r.attest(None);
    let (code, _, err) = r.verify_args(WORKFLOW, &["--source-ref", DEV_REF], None);
    assert_eq!(code, Some(1), "{err}");
}

#[test]
fn a_source_ref_that_is_not_a_branch_of_the_repository_is_refused() {
    // Catches: `--source-ref` taken as free text, so a verification could be pointed at
    // a tag, a pull request ref or an empty value and pass.
    let r = Release::build("bad-ref");
    r.attest(None);
    for bad in [
        "",
        "main",
        "refs/tags/v1",
        "refs/pull/1/merge",
        "refs/heads/",
    ] {
        let (code, _, err) = r.verify_args(WORKFLOW, &["--source-ref", bad], None);
        assert_eq!(code, Some(2), "{bad:?}: {err}");
        assert!(
            err.contains("--source-ref") && err.contains("refs/heads/"),
            "{bad:?}: {err}"
        );
    }
    assert_eq!(
        std::fs::read_to_string(r.gh.with_file_name("gh-args")).unwrap_or_default(),
        "",
        "gh was run for a refused ref"
    );
}

#[test]
fn a_verifier_that_accepts_any_signer_is_caught_by_the_signer_control() {
    // Catches: the signer control skipped, or run with the signing workflow: a gh that
    // verifies everything must fail the run, not pass it.
    let r = Release::build("accepts-all");
    // No attestation is recorded: the fake accepts all. The bundles are there, as the
    // check wants them before it gets to the controls.
    r.bundles("refs/heads/main", COMMIT);
    let (code, out, err) = r.verify(WORKFLOW, Some("accepts-all"));
    assert_eq!(code, Some(1), "{out}");
    assert_eq!(
        err,
        format!(
            "xtask: control failed: {} verified as signed by {REPO}/.github/workflows/ci.yml\n",
            first_tarball()
        )
    );
}

#[test]
fn a_verifier_that_ignores_the_bytes_is_caught_by_the_flipped_byte_control() {
    // Catches: the byte-flip control skipped: a gh that checks the flags but not which
    // file an attestation covers passes every check above it, so only the flipped
    // copy can fail the run.
    let r = Release::build("ignores-bytes");
    r.attest(None);
    let (code, out, err) = r.verify(WORKFLOW, Some("ignores-bytes"));
    assert_eq!(code, Some(1), "{out}");
    assert_eq!(
        err,
        format!(
            "xtask: control failed: a flipped byte in {} still verified\n",
            first_tarball()
        )
    );
}

#[test]
fn a_missing_attestation_fails_the_run_and_names_it() {
    // Catches: a tarball's SBOM attestation, or an SBOM's provenance, not checked or a
    // failed check passed over.
    let r = Release::build("missing");
    let tarball = format!("kbf-server-{COMMIT}-linux-arm64.tar.gz");
    let sbom = format!("kbf-server-{COMMIT}-linux-x86_64.cdx.json");
    for (name, is_sbom, what) in [
        (&tarball, true, "https://cyclonedx.org/bom"),
        (&tarball, false, "build provenance"),
        (&sbom, false, "build provenance"),
    ] {
        r.attest(Some((name.as_str(), is_sbom)));
        let (code, _, err) = r.verify(WORKFLOW, None);
        assert_eq!(code, Some(1), "{name} {what}");
        assert_eq!(
            err,
            format!(
                "xtask: {}: no {what} attestation signed by {WORKFLOW} verifies\n",
                s(&r.dist.join(name))
            )
        );
    }
}

#[test]
fn attestations_refuse_a_foreign_workflow_an_unknown_file_and_a_missing_gh() {
    // Catches: a --workflow-ref of another repository accepted (it would be the
    // signer every check trusts), a listed file of neither kind passed unchecked, and
    // a gh that cannot start reported as an attestation that did not verify.
    let r = Release::build("refusals");
    r.attest(None);
    let (code, _, err) = r.verify("someone/else/.github/workflows/a.yml@refs/heads/main", None);
    assert_eq!(code, Some(2));
    assert_eq!(
        err,
        format!(
            "xtask: --workflow-ref \"someone/else/.github/workflows/a.yml@refs/heads/main\" \
             is not a workflow of {REPO}\n"
        )
    );
    let missing = r.gh.with_file_name("no-gh");
    let (code, _, err) = xtask(
        &[
            "verify-assets",
            "attestations",
            s(&r.dist),
            "--repo",
            REPO,
            "--workflow-ref",
            WORKFLOW,
            "--commit",
            COMMIT,
            "--gh",
            s(&missing),
        ],
        &r.tmp,
        None,
    );
    assert_eq!(code, Some(1));
    assert!(
        err.starts_with(&format!("xtask: {}: ", s(&missing))),
        "{err}"
    );
    let notes = "notes.txt";
    std::fs::write(r.dist.join(notes), b"notes").expect("notes");
    let digest = hex::encode(sha2::Sha256::digest(b"notes"));
    let mut sums = std::fs::read_to_string(r.dist.join("SHA256SUMS")).expect("sums");
    sums.push_str(&format!("{digest}  {notes}\n"));
    std::fs::write(r.dist.join("SHA256SUMS"), sums).expect("sums");
    let (code, _, err) = r.verify(WORKFLOW, None);
    assert_eq!(code, Some(1));
    assert_eq!(
        err,
        "xtask: SHA256SUMS lists notes.txt, neither a tarball, a package, an SBOM \
         nor facts\n"
    );
}

#[test]
fn a_kbf_tarball_without_its_version_facts_fails_the_sums_check() {
    // Catches: a release set that does not need the facts of its `kbf` (a build job that
    // lost the step would publish a platform with nothing for a reader of the platform's facts), and the
    // message not naming the tarball and the file missing. The second half is the control:
    // with the facts listed and present the same set passes, so the refusal was for them
    // and for nothing else, and `kbf-daemon`'s tarball (no facts) is not named. Catches:
    // the pairing taken out of `verify_sums`.
    let names: &[&str] = &["kbf", "kbf-daemon"];
    let r = Release::build_with("facts-missing", &[("linux-x86_64", names)]);
    let tarball = format!("kbf-{COMMIT}-linux-x86_64.tar.gz");
    let facts = format!("kbf-{COMMIT}-linux-x86_64.version.json");
    let (code, out, err) = xtask(&["verify-assets", "sums", s(&r.dist)], &r.tmp, None);
    assert_eq!(code, Some(1), "{out}");
    let want = format!(
        "xtask: {tarball}: its version facts {facts} are not listed in \
         SHA256SUMS\n"
    );
    assert_eq!(err, want);
    r.add_facts("linux-x86_64");
    let (code, out, err) = xtask(&["verify-assets", "sums", s(&r.dist)], &r.tmp, None);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(out, "sums: 5 files match; the flipped copy does not\n");
}

#[test]
fn version_facts_without_their_kbf_tarball_fail_the_sums_check() {
    // Catches: facts in the release set of a platform that has no `kbf` there (a stray
    // file, or another build's, would be attested and carried as if it stated something of
    // a binary that ships), and a message that does not name both files. The control is
    // the pairing of the test above, with the tarball listed. Catches: the reverse pairing
    // taken out of `verify_sums`.
    let names: &[&str] = &["kbf-daemon"];
    let r = Release::build_with("facts-orphan", &[("linux-x86_64", names)]);
    let facts = r.add_facts("linux-x86_64");
    let (code, out, err) = xtask(&["verify-assets", "sums", s(&r.dist)], &r.tmp, None);
    assert_eq!(code, Some(1), "{out}");
    let want = format!(
        "xtask: {facts}: the tarball it states the facts of, \
         kbf-{COMMIT}-linux-x86_64.tar.gz, is not listed in SHA256SUMS\n"
    );
    assert_eq!(err, want);
}

#[test]
fn version_facts_are_verified_for_build_provenance_and_nothing_else() {
    // Catches: facts the attest job signs that the verify job refuses as neither a tarball,
    // a package nor an SBOM; facts held to what only a tarball has (an SBOM attestation, a
    // bundle: neither exists, and the release check names a bundle of anything else); and
    // facts whose provenance is not checked, or whose missing attestation is passed over.
    let names: &[&str] = &["kbf", "kbf-daemon"];
    let r = Release::build_with("facts-attested", &[("linux-x86_64", names)]);
    let facts = r.add_facts("linux-x86_64");
    r.attest(None);
    let (code, out, err) = r.verify(&format!("{WORKFLOW}@refs/heads/main"), None);
    assert_eq!(code, Some(0), "{err}");
    let line = format!("verified: {facts} (provenance)\n");
    assert!(out.contains(&line), "{out}");
    let path = s(&r.dist.join(&facts)).to_owned();
    let calls = r.gh_calls();
    let mine: Vec<&String> = calls.iter().filter(|c| c.contains(&path)).collect();
    assert_eq!(mine.len(), 1, "{mine:#?}");
    assert_eq!(
        *mine[0],
        format!("attestation verify {path} {}", flags(WORKFLOW))
    );
    r.attest(Some((facts.as_str(), false)));
    let (code, _, err) = r.verify(WORKFLOW, None);
    assert_eq!(code, Some(1), "{err}");
    let want = format!(
        "xtask: {path}: no build provenance attestation signed by \
         {WORKFLOW} verifies\n"
    );
    assert_eq!(err, want);
}

#[test]
fn a_task_image_does_not_go_without_its_facts_or_the_facts_without_the_image() {
    // Catches: a release set that lists the task image without the facts that carry the digests
    // of its manifest and base (the attestation would not cover them), and facts left in a set
    // with no image they state anything of. The control is the set with both listed.
    let names: &[&str] = &["kbf-daemon"];
    let r = Release::build_with("image-facts", &[("linux-x86_64", names)]);
    let image = format!("kbf-task-image-{COMMIT}-linux-x86_64.oci.tar");
    let facts = format!("kbf-task-image-{COMMIT}-linux-x86_64.facts.json");
    let sbom = format!("kbf-task-image-{COMMIT}-linux-x86_64.cdx.json");
    r.add_listed(&image, "the task image\n");
    r.add_listed(&sbom, "the SBOM of the task image\n");
    let (code, out, err) = xtask(&["verify-assets", "sums", s(&r.dist)], &r.tmp, None);
    assert_eq!(code, Some(1), "{out}");
    assert_eq!(
        err,
        format!("xtask: {image}: its image facts {facts} are not listed in SHA256SUMS\n")
    );
    r.add_listed(&facts, "the facts of the task image\n");
    let (code, out, err) = xtask(&["verify-assets", "sums", s(&r.dist)], &r.tmp, None);
    assert_eq!(code, Some(0), "{err}");
    assert!(out.starts_with("sums: "), "{out}");

    let r = Release::build_with("image-facts-orphan", &[("linux-x86_64", names)]);
    r.add_listed(&facts, "the facts of the task image\n");
    let (code, out, err) = xtask(&["verify-assets", "sums", s(&r.dist)], &r.tmp, None);
    assert_eq!(code, Some(1), "{out}");
    assert_eq!(
        err,
        format!(
            "xtask: {facts}: the image it states the facts of, {image}, is not listed in \
             SHA256SUMS\n"
        )
    );
}

#[test]
fn a_task_image_does_not_go_without_its_sbom_or_the_sbom_without_the_image() {
    // Catches: a release set that lists the task image without the CycloneDX SBOM of the image (the
    // base's packages would be attested nowhere), an image SBOM left in a set with no image it
    // describes, the SBOM of the `kbf-task` binary (`kbf-task-<commit>-<platform>.cdx.json`)
    // mistaken for the image's, and a pairing that is checked for one platform only. The control
    // is the set with both listed.
    let names: &[&str] = &["kbf-daemon"];
    let r = Release::build_with("image-sbom", &[("linux-x86_64", names)]);
    let image = format!("kbf-task-image-{COMMIT}-linux-x86_64.oci.tar");
    let facts = format!("kbf-task-image-{COMMIT}-linux-x86_64.facts.json");
    let sbom = format!("kbf-task-image-{COMMIT}-linux-x86_64.cdx.json");
    r.add_listed(&image, "the task image\n");
    r.add_listed(&facts, "the facts of the task image\n");
    let (code, out, err) = xtask(&["verify-assets", "sums", s(&r.dist)], &r.tmp, None);
    assert_eq!(code, Some(1), "{out}");
    assert_eq!(
        err,
        format!("xtask: {image}: its image SBOM {sbom} is not listed in SHA256SUMS\n")
    );
    r.add_listed(&sbom, "the SBOM of the task image\n");
    let (code, out, err) = xtask(&["verify-assets", "sums", s(&r.dist)], &r.tmp, None);
    assert_eq!(code, Some(0), "{err}");
    assert!(out.starts_with("sums: "), "{out}");

    let r = Release::build_with("image-sbom-orphan", &[("linux-x86_64", names)]);
    r.add_listed(&sbom, "the SBOM of the task image\n");
    let (code, out, err) = xtask(&["verify-assets", "sums", s(&r.dist)], &r.tmp, None);
    assert_eq!(code, Some(1), "{out}");
    assert_eq!(
        err,
        format!("xtask: {sbom}: the image it describes, {image}, is not listed in SHA256SUMS\n")
    );

    // The SBOM of the binary is the tarball's, and is no image's.
    let r = Release::build_with("image-sbom-binary", &[("linux-arm64", &["kbf-task"][..])]);
    let (code, out, err) = xtask(&["verify-assets", "sums", s(&r.dist)], &r.tmp, None);
    assert_eq!(code, Some(0), "{err}: {out}");
}

#[test]
fn the_task_image_is_verified_with_an_sbom_attestation_and_its_sbom_with_provenance() {
    // Catches: an image whose SBOM attestation the verify job never asks for (the attest job could
    // lose its step and nothing would notice); the SBOM asked for with provenance only, or the
    // image's SBOM attestation required of an image that has no SBOM listed (the previous release);
    // the image's bundle dropped because it now has an SBOM attestation too; and the SBOM file
    // refused as neither a tarball, a package nor facts.
    let names: &[&str] = &["kbf", "kbf-daemon"];
    let r = Release::build_with("image-sbom-attested", &[("linux-x86_64", names)]);
    let image = format!("kbf-task-image-{COMMIT}-linux-x86_64.oci.tar");
    let facts = format!("kbf-task-image-{COMMIT}-linux-x86_64.facts.json");
    let sbom = format!("kbf-task-image-{COMMIT}-linux-x86_64.cdx.json");
    for (name, what) in [
        (&image, "the task image\n"),
        (&facts, "the facts of the task image\n"),
        (&sbom, "the SBOM of the task image\n"),
    ] {
        r.add_listed(name, what);
    }
    r.attest(None);
    let (code, out, err) = r.verify(&format!("{WORKFLOW}@refs/heads/main"), None);
    assert_eq!(code, Some(0), "{err}");
    assert!(
        out.lines()
            .any(|l| l == format!("verified: {image} (provenance, SBOM, bundle)")),
        "{out}"
    );
    assert!(
        out.lines()
            .any(|l| l == format!("verified: {sbom} (provenance)")),
        "{out}"
    );
    let path = s(&r.dist.join(&image)).to_owned();
    let calls = r.gh_calls();
    let asked = format!(
        "attestation verify {path} {} {SBOM_PREDICATE}",
        flags(WORKFLOW)
    );
    assert_eq!(
        calls.iter().filter(|c| **c == asked).count(),
        1,
        "{calls:#?}"
    );
    // The image's SBOM attestation lost: refused, naming the predicate.
    r.attest(Some((image.as_str(), true)));
    let (code, _, err) = r.verify(WORKFLOW, None);
    assert_eq!(code, Some(1), "{err}");
    assert_eq!(
        err,
        format!(
            "xtask: {path}: no https://cyclonedx.org/bom attestation signed by {WORKFLOW} verifies\n"
        )
    );
    // The SBOM file's own provenance lost: refused as any file's.
    r.attest(Some((sbom.as_str(), false)));
    let (code, _, err) = r.verify(WORKFLOW, None);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains(&sbom), "{err}");
}

#[test]
fn a_task_image_with_no_sbom_listed_is_verified_as_before() {
    // Catches: the SBOM attestation required of a task image whose SBOM the set does not list (the
    // previous release, made before the image had one, is read by `release fetch-previous`, which
    // verifies its attestations): provenance and the bundle only.
    let names: &[&str] = &["kbf", "kbf-daemon"];
    let r = Release::build_with("image-no-sbom", &[("linux-x86_64", names)]);
    let image = format!("kbf-task-image-{COMMIT}-linux-x86_64.oci.tar");
    let facts = format!("kbf-task-image-{COMMIT}-linux-x86_64.facts.json");
    r.add_listed(&image, "the task image\n");
    r.add_listed(&facts, "the facts of the task image\n");
    r.attest(None);
    let (code, out, err) = r.verify(&format!("{WORKFLOW}@refs/heads/main"), None);
    assert_eq!(code, Some(0), "{err}");
    assert!(
        out.lines()
            .any(|l| l == format!("verified: {image} (provenance, bundle)")),
        "{out}"
    );
}

#[test]
fn the_facts_of_the_task_image_are_verified_for_build_provenance_and_nothing_else() {
    // Catches: image facts the attest job signs through the sums that the verify job refuses as
    // neither a tarball, a package nor an SBOM; facts held to what only a tarball has (an SBOM
    // attestation, a bundle: neither exists); and facts whose provenance is not checked, or
    // whose missing attestation is passed over.
    let names: &[&str] = &["kbf", "kbf-daemon"];
    let r = Release::build_with("image-facts-attested", &[("linux-x86_64", names)]);
    let facts = format!("kbf-task-image-{COMMIT}-linux-x86_64.facts.json");
    r.add_listed(&facts, "the facts of the task image\n");
    r.attest(None);
    let (code, out, err) = r.verify(&format!("{WORKFLOW}@refs/heads/main"), None);
    assert_eq!(code, Some(0), "{err}");
    let line = format!("verified: {facts} (provenance)\n");
    assert!(out.contains(&line), "{out}");
    let path = s(&r.dist.join(&facts)).to_owned();
    let calls = r.gh_calls();
    let mine: Vec<&String> = calls.iter().filter(|c| c.contains(&path)).collect();
    assert_eq!(mine.len(), 1, "{mine:#?}");
    r.attest(Some((facts.as_str(), false)));
    let (code, _, err) = r.verify(WORKFLOW, None);
    assert_eq!(code, Some(1), "{err}");
    let want = format!(
        "xtask: {path}: no build provenance attestation signed by \
         {WORKFLOW} verifies\n"
    );
    assert_eq!(err, want);
}

#[test]
fn verify_sums_fails_the_run_on_a_changed_asset() {
    // Catches: the CLI's sums check passing a directory whose asset changed after it
    // was summed (the exit status, not only the message, must say so).
    let r = Release::build("changed");
    let tarball = r.dist.join(first_tarball());
    let mut bytes = std::fs::read(&tarball).expect("tarball");
    bytes[0] ^= 1;
    std::fs::write(&tarball, bytes).expect("tarball");
    let (code, out, err) = xtask(&["verify-assets", "sums", s(&r.dist)], &r.tmp, None);
    assert_eq!(code, Some(1), "{out}");
    assert_eq!(
        err,
        format!("xtask: {}: does not match its line\n", first_tarball())
    );
}

#[test]
fn a_bundle_is_verified_offline_with_the_installers_flags() {
    // Catches: the bundle check missing a flag the installer passes (the fake verifies
    // only the exact flags: `--source-digest`, `--cert-oidc-issuer`, `--predicate-type`
    // and `--deny-self-hosted-runners` each); the trusted root not handed on as
    // `--custom-trusted-root`; and a "offline" check that can reach the network (with a
    // trusted root every bundle call runs under a proxy nothing listens on, as the
    // installer's does; without one nothing is planted).
    let r = Release::build("bundle-offline");
    r.attest(None);
    let root = r.dist.parent().expect("root").join("trusted_root.jsonl");
    std::fs::write(&root, "{}\n").expect("trusted root");
    let (code, out, err) = r.verify_args(WORKFLOW, &["--trusted-root", s(&root)], None);
    assert_eq!(code, Some(0), "{err}: {out}");
    let calls = r.bundle_calls();
    // Five tarballs, then the three controls that use a bundle.
    assert_eq!(calls.len(), 8, "{calls:#?}");
    let first = r.dist.join(first_tarball());
    let bundle = r.bundle_of(&first_tarball());
    assert_eq!(
        calls[0],
        format!(
            "attestation verify {} --bundle {} --custom-trusted-root {} {}",
            s(&first),
            s(&bundle),
            s(&root),
            bundle_flags("refs/heads/main", COMMIT)
        )
    );
    // Every spelling of the proxy variables points at the dead proxy, and the runner's
    // `NO_PROXY=*` (the test sets it for every call) is gone: a bypass list would let
    // the call past the proxy to the network.
    let dead = format!(
        "HTTPS_PROXY={DEAD_PROXY} https_proxy={DEAD_PROXY} HTTP_PROXY={DEAD_PROXY} \
         http_proxy={DEAD_PROXY} ALL_PROXY={DEAD_PROXY} all_proxy={DEAD_PROXY} \
         NO_PROXY= no_proxy="
    );
    assert_eq!(r.bundle_proxies(), vec![dead; 8]);

    let r = Release::build("bundle-online");
    r.attest(None);
    let (code, out, err) = r.verify(WORKFLOW, None);
    assert_eq!(code, Some(0), "{err}: {out}");
    let calls = r.bundle_calls();
    assert_eq!(calls.len(), 8, "{calls:#?}");
    assert!(
        calls.iter().all(|c| !c.contains("--custom-trusted-root")),
        "{calls:#?}"
    );
    assert!(
        r.bundle_proxies()
            .iter()
            .all(|p| !p.contains(DEAD_PROXY) && p.contains("NO_PROXY=* no_proxy=*")),
        "a proxy was planted, or the runner's environment changed, with no trusted root: {:?}",
        r.bundle_proxies()
    );
}

#[test]
fn a_tarball_without_its_bundle_is_named() {
    // Catches: a tarball verified from GitHub's store alone, so a release that lost a
    // bundle passes every check here and leaves a host nothing to verify it with.
    let r = Release::build("no-bundle");
    r.attest(None);
    let t = second_tarball();
    std::fs::remove_file(r.bundle_of(&t)).expect("remove");
    let (code, out, err) = r.verify(WORKFLOW, None);
    assert_eq!(code, Some(1), "{out}");
    assert_eq!(
        err,
        format!("xtask: {t}: no provenance bundle {t}.sigstore.json\n")
    );
}

#[test]
fn a_bundle_that_does_not_verify_for_its_tarball_fails_the_run_and_names_it() {
    // Catches: a bundle taken on its name: one signed for another commit (a release of
    // the wrong commit's bundles), one from another branch than the one verified, and
    // two tarballs' bundles swapped (each is a valid bundle, of the other file). The
    // message names the tarball, its bundle and what gh said, which is all there is
    // to read when a first push fails.
    let head = |r: &Release| {
        format!(
            "xtask: {}: its bundle {}.sigstore.json does not verify: ",
            s(&r.dist.join(first_tarball())),
            first_tarball()
        )
    };
    let r = Release::build("bundle-other-commit");
    r.attest(None);
    r.bundles("refs/heads/main", OTHER);
    let (code, _, err) = r.verify(WORKFLOW, None);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.starts_with(&head(&r)), "{err}");
    assert!(err.contains(&format!("--source-digest {COMMIT}")), "{err}");

    let r = Release::build("bundle-other-branch");
    r.attest(None);
    r.bundles(DEV_REF, COMMIT);
    let (code, _, err) = r.verify(WORKFLOW, None);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.starts_with(&head(&r)), "{err}");
    assert!(err.contains("--source-ref refs/heads/main"), "{err}");

    // Two bundles that name the right tarballs but carry each other's signatures: the
    // payload says one thing and the signature covers another, which only gh can see.
    let r = Release::build("bundle-forged-payload");
    r.attest(None);
    let (a, b) = (
        r.bundle_of(&first_tarball()),
        r.bundle_of(&second_tarball()),
    );
    let read = |p: &Path| -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(p).expect("bundle")).expect("json")
    };
    let (mut json_a, mut json_b) = (read(&a), read(&b));
    std::mem::swap(
        &mut json_a["fakeAttestations"],
        &mut json_b["fakeAttestations"],
    );
    std::fs::write(&a, format!("{json_a}\n")).expect("bundle");
    std::fs::write(&b, format!("{json_b}\n")).expect("bundle");
    let (code, _, err) = r.verify(WORKFLOW, None);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.starts_with(&head(&r)), "{err}");
}

#[test]
fn two_swapped_bundles_are_refused_by_their_subjects_before_gh_is_asked() {
    // Catches: a verification that leaves the comparison of a bundle's subject digest to
    // gh alone, so a bundle copied under another tarball's name costs a gh call per
    // tarball and the error is gh's rather than the two names; and a swap the check
    // names only one side of.
    let r = Release::build("bundle-swapped");
    r.attest(None);
    let (a, b) = (
        r.bundle_of(&first_tarball()),
        r.bundle_of(&second_tarball()),
    );
    let (text_a, text_b) = (
        std::fs::read(&a).expect("bundle"),
        std::fs::read(&b).expect("bundle"),
    );
    std::fs::write(&a, text_b).expect("bundle");
    std::fs::write(&b, text_a).expect("bundle");
    let sums = r.sums();
    let sha = |name: &str| {
        sums.iter()
            .find(|(_, n)| n == name)
            .expect("listed")
            .0
            .clone()
    };
    let (code, _, err) = r.verify(WORKFLOW, None);
    assert_eq!(code, Some(1), "{err}");
    assert_eq!(
        err,
        format!(
            "xtask: {}.sigstore.json: has the subject digest sha256:{}, not the tarball's \
             sha256:{}\n{}.sigstore.json: has the subject digest sha256:{}, not the tarball's \
             sha256:{}\n",
            first_tarball(),
            sha(&second_tarball()),
            sha(&first_tarball()),
            second_tarball(),
            sha(&first_tarball()),
            sha(&second_tarball()),
        )
    );
    assert_eq!(
        std::fs::read_to_string(r.gh.with_file_name("gh-args")).unwrap_or_default(),
        "",
        "gh was run for a directory whose bundles are not its tarballs'"
    );
}

#[test]
fn a_bundle_no_listed_tarball_owns_is_refused_in_the_verify_job_too() {
    // Catches: strays found only at release time. The verify job writes its bundles
    // itself, but a name that outlives a rename (a tarball no job packages any more,
    // its bundle still uploaded) or a copy left in the directory would pass every check
    // here and fail the release that takes the same directory. Refused before gh is
    // asked, naming the file.
    let r = Release::build("bundle-stray");
    r.attest(None);
    let stray = format!("kbf-old-{COMMIT}-linux-x86_64.tar.gz.sigstore.json");
    std::fs::copy(r.bundle_of(&first_tarball()), r.dist.join(&stray)).expect("stray");
    let (code, out, err) = r.verify(WORKFLOW, None);
    assert_eq!(code, Some(1), "{out}");
    assert_eq!(
        err,
        format!("xtask: {stray}: not the bundle of a listed tarball\n")
    );
    assert_eq!(
        std::fs::read_to_string(r.gh.with_file_name("gh-args")).unwrap_or_default(),
        "",
        "gh was run for a directory with a stray bundle"
    );
}

#[test]
fn a_bundle_verifier_that_ignores_the_commit_or_the_tarball_is_caught_by_the_controls() {
    // Catches: each control skipped. A gh that does not compare `--source-digest` passes
    // every bundle above it, so only the same bundle asked for another commit can fail
    // the run; one that does not compare the file's digest is caught by another
    // tarball's bundle (the control that also shows a bundle is one tarball's own, not
    // a shared one), and, where there is one tarball only, by the flipped copy.
    let r = Release::build("bundle-any-commit");
    r.attest(None);
    let (code, out, err) = r.verify(WORKFLOW, Some("bundle-any-commit"));
    assert_eq!(code, Some(1), "{out}");
    assert_eq!(
        err,
        format!(
            "xtask: control failed: the bundle of {} verified for commit {}, not the commit \
             it attests\n",
            first_tarball(),
            control_commit(COMMIT)
        )
    );

    let r = Release::build("bundle-any-file");
    r.attest(None);
    let (code, out, err) = r.verify(WORKFLOW, Some("bundle-any-file"));
    assert_eq!(code, Some(1), "{out}");
    assert_eq!(
        err,
        format!(
            "xtask: control failed: the bundle of {} verified {}: a bundle is to attest its \
             own tarball only\n",
            second_tarball(),
            first_tarball()
        )
    );

    // One tarball: no other bundle to try, so the flipped copy is the control.
    let one = [("linux-x86_64", &["kbf-daemon"][..])];
    let r = Release::build_with("bundle-flipped", &one);
    r.attest(None);
    let tarball = format!("kbf-daemon-{COMMIT}-linux-x86_64.tar.gz");
    let (code, out, err) = r.verify(WORKFLOW, None);
    assert_eq!(code, Some(0), "{err}: {out}");
    // The tarball itself and the two controls that use its bundle.
    assert_eq!(r.bundle_calls().len(), 3, "{:#?}", r.bundle_calls());
    let (code, out, err) = r.verify(WORKFLOW, Some("bundle-any-file"));
    assert_eq!(code, Some(1), "{out}");
    assert_eq!(
        err,
        format!(
            "xtask: control failed: a flipped byte in {tarball} still verified against its bundle\n"
        )
    );
}

#[test]
fn the_commit_a_bundle_is_checked_for_must_be_a_full_sha() {
    // Catches: `--commit` taken as free text (a short SHA, a branch name or an empty
    // value would be handed to `--source-digest`, which gh compares as written), or
    // optional (a verification that does not pin the commit at all). gh is not run.
    let r = Release::build("bad-commit");
    r.attest(None);
    for bad in ["", "abc", &COMMIT[..12], "main", &"g".repeat(40)] {
        let (code, _, err) = r.verify_commit(WORKFLOW, bad, &[], None);
        assert_eq!(code, Some(2), "{bad:?}: {err}");
        assert!(
            err.contains(&format!("--commit {bad:?} is not a full commit SHA")),
            "{bad:?}: {err}"
        );
    }
    let (code, _, err) = xtask(
        &[
            "verify-assets",
            "attestations",
            s(&r.dist),
            "--repo",
            REPO,
            "--workflow-ref",
            WORKFLOW,
            "--gh",
            s(&r.gh),
        ],
        &r.tmp,
        None,
    );
    assert_eq!(code, Some(2), "{err}");
    assert!(err.contains("--commit"), "{err}");
    assert_eq!(
        std::fs::read_to_string(r.gh.with_file_name("gh-args")).unwrap_or_default(),
        "",
        "gh was run for a refused commit"
    );
}

/// A release of one tarball and the kbf-installer deb of the same platform, attested as
/// the attest job leaves them: the deb has build provenance and a bundle, no SBOM.
fn with_deb(name: &str) -> (Release, String) {
    let one = [("linux-x86_64", &["kbf-installer"][..])];
    let r = Release::build_with(name, &one);
    let deb = r.add_deb("linux-x86_64");
    r.attest(None);
    (r, deb)
}

#[test]
fn the_mac_package_is_verified_like_the_deb_for_provenance_and_its_own_bundle() {
    // Catches: a `.pkg` in the macOS SHA256SUMS refused as "neither a tarball, a package
    // nor an SBOM" (verify-darwin-arm64 failing on the first push that lists one), a `.pkg`
    // verified without its bundle, asked for an SBOM attestation it does not have, and the
    // package controls (its bundle verifying the tarball and the reverse) not run for it.
    let one = [("darwin-arm64", &["kbf-installer"][..])];
    let r = Release::build_with("pkg-good", &one);
    let pkg = r.add_package("darwin-arm64", "pkg");
    r.attest(None);
    let (code, out, err) = r.verify(WORKFLOW, None);
    assert_eq!(code, Some(0), "{err}: {out}");
    assert!(
        out.lines()
            .any(|l| l == format!("verified: {pkg} (provenance, bundle)")),
        "{out}"
    );
    let calls = r.gh_calls();
    let path = r.dist.join(&pkg);
    let on_pkg: Vec<&String> = calls
        .iter()
        .filter(|c| c.starts_with(&format!("attestation verify {} ", s(&path))))
        .collect();
    assert_eq!(on_pkg.len(), 4, "{on_pkg:#?}");
    assert!(
        on_pkg.iter().all(|c| !c.contains(SBOM_PREDICATE)),
        "{on_pkg:#?}"
    );
    // Its bundle is required: removed, the listing is refused.
    std::fs::remove_file(r.bundle_of(&pkg)).expect("remove");
    let (code, _, err) = r.verify(WORKFLOW, None);
    assert_eq!(code, Some(1));
    assert!(
        err.contains(&format!("{pkg}: no provenance bundle")),
        "{err}"
    );
}

#[test]
fn a_package_is_verified_for_provenance_and_its_own_bundle_and_for_no_sbom() {
    // Catches: a deb in SHA256SUMS refused as "neither a tarball nor an SBOM" (the
    // verifier and so the verify-linux job failing on the first push that lists one), a
    // deb verified without its bundle (an unsigned bundle a host would rely on), one
    // asked for an SBOM attestation it does not have, the bundle checked with other
    // flags than the installer's, and the package controls not run (the deb's bundle
    // verifying the tarball, the tarball verifying under the deb's bundle).
    let (r, deb) = with_deb("deb-good");
    let (code, out, err) = r.verify(WORKFLOW, None);
    assert_eq!(code, Some(0), "{err}: {out}");
    let lines: Vec<&str> = out.lines().collect();
    assert!(
        lines.contains(&format!("verified: {deb} (provenance, bundle)").as_str()),
        "{out}"
    );
    let calls = r.gh_calls();
    let deb_path = r.dist.join(&deb);
    let file = s(&deb_path);
    let verifying = format!("attestation verify {file} ");
    let on_deb: Vec<&String> = calls.iter().filter(|c| c.starts_with(&verifying)).collect();
    // Provenance, its bundle, and the controls: against the tarball's bundle, against
    // its own for another commit. (The tarball against the deb's bundle has the tarball
    // as its file.)
    assert_eq!(on_deb.len(), 4, "{on_deb:#?}");
    assert_eq!(
        on_deb[0],
        &format!("attestation verify {file} {}", flags(WORKFLOW))
    );
    let bundle = r.bundle_of(&deb);
    assert_eq!(
        on_deb[1],
        &format!(
            "attestation verify {file} --bundle {} {}",
            s(&bundle),
            bundle_flags("refs/heads/main", COMMIT)
        )
    );
    assert!(
        on_deb.iter().all(|c| !c.contains(SBOM_PREDICATE)),
        "an SBOM attestation was asked of the deb: {on_deb:#?}"
    );
    assert!(
        calls
            .iter()
            .any(|c| c.contains(&format!(".tar.gz --bundle {} ", s(&bundle)))),
        "the tarball was not tried against the deb's bundle: {calls:#?}"
    );
}

#[test]
fn a_package_without_its_bundle_or_with_anothers_is_refused_before_gh_is_asked() {
    // Catches: the pairing blind to a deb (a package released with no bundle a host can
    // verify it with, a stray bundle of an unlisted deb, or the tarball's bundle copied
    // under the deb's name, which only the subject comparison finds).
    let (r, deb) = with_deb("deb-bundles");
    std::fs::remove_file(r.bundle_of(&deb)).expect("remove");
    let (code, out, err) = r.verify(WORKFLOW, None);
    assert_eq!(code, Some(1), "{out}");
    assert_eq!(
        err,
        format!("xtask: {deb}: no provenance bundle {deb}.sigstore.json\n")
    );

    let (r, deb) = with_deb("deb-swapped");
    let tarball = format!("kbf-installer-{COMMIT}-linux-x86_64.tar.gz");
    std::fs::copy(r.bundle_of(&tarball), r.bundle_of(&deb)).expect("swap");
    let (code, out, err) = r.verify(WORKFLOW, None);
    assert_eq!(code, Some(1), "{out}");
    assert!(
        err.starts_with(&format!(
            "xtask: {deb}.sigstore.json: has the subject digest sha256:"
        )),
        "{err}"
    );

    let (r, _) = with_deb("deb-stray");
    let stray = format!("kbf-installer-{COMMIT}-linux-arm64.deb.sigstore.json");
    std::fs::copy(
        r.bundle_of(&format!("kbf-installer-{COMMIT}-linux-x86_64.deb")),
        r.dist.join(&stray),
    )
    .expect("stray");
    let (code, out, err) = r.verify(WORKFLOW, None);
    assert_eq!(code, Some(1), "{out}");
    assert_eq!(
        err,
        format!("xtask: {stray}: not the bundle of a listed tarball\n")
    );
    assert_eq!(
        std::fs::read_to_string(r.gh.with_file_name("gh-args")).unwrap_or_default(),
        "",
        "gh was run before the pairing was checked"
    );
}

#[test]
fn a_package_bundle_that_does_not_verify_fails_the_run_and_names_it() {
    // Catches: the deb's own bundle never put to gh (only its provenance in GitHub's
    // store), so a bundle signed for another commit, or by no one, would ship.
    let (r, deb) = with_deb("deb-bad-bundle");
    let sha = hex::encode(sha2::Sha256::digest(
        "the kbf-installer package for linux-x86_64",
    ));
    std::fs::write(
        r.bundle_of(&deb),
        bundle_json(&sha, &bundle_flags("refs/heads/main", OTHER)),
    )
    .expect("bundle");
    let (code, out, err) = r.verify(WORKFLOW, None);
    assert_eq!(code, Some(1), "{out}");
    assert!(
        err.starts_with(&format!(
            "xtask: {}: its bundle {deb}.sigstore.json does not verify: ",
            s(&r.dist.join(&deb))
        )),
        "{err}"
    );
}

#[test]
fn a_verifier_that_mixes_package_and_tarball_bundles_is_caught_by_the_package_controls() {
    // Catches: each package control skipped. With a gh that verifies any file against a
    // bundle, the deb passes the tarball's bundle; with one that takes any file under a
    // deb's bundle, the tarball passes the deb's; with one that ignores the commit, the
    // deb passes for another commit. Each message names the package.
    let (r, deb) = with_deb("deb-controls-file");
    let tarball = format!("kbf-installer-{COMMIT}-linux-x86_64.tar.gz");
    let (code, out, err) = r.verify(WORKFLOW, Some("bundle-any-file"));
    assert_eq!(code, Some(1), "{out}");
    assert_eq!(
        err,
        format!("xtask: control failed: the bundle of {tarball} verified the package {deb}\n")
    );
    let (r, deb) = with_deb("deb-controls-bundle");
    let (code, out, err) = r.verify(WORKFLOW, Some("bundle-deb-bundle-any"));
    assert_eq!(code, Some(1), "{out}");
    assert_eq!(
        err,
        format!("xtask: control failed: the bundle of the package {deb} verified {tarball}\n")
    );
    let (r, deb) = with_deb("deb-controls-commit");
    let (code, out, err) = r.verify(WORKFLOW, Some("bundle-any-commit"));
    assert_eq!(code, Some(1), "{out}");
    assert_eq!(
        err,
        format!(
            "xtask: control failed: the bundle of {deb} verified for commit {}, not the commit it attests\n",
            control_commit(COMMIT)
        )
    );
}
