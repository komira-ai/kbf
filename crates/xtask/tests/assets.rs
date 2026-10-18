//! `cargo xtask package`, `sums assemble` and `verify-assets` run as the artifacts
//! workflow runs them, with `xtask-fake` as `gh` and its file `attested` as the store
//! of attestations.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use sha2::Digest;

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
const FAKE: &str = env!("CARGO_BIN_EXE_xtask-fake");
const COMMIT: &str = "c0ffee0123456789c0ffee0123456789c0ffee01";
const REPO: &str = "komira-ai/komira-build-farm";
const WORKFLOW: &str = "komira-ai/komira-build-farm/.github/workflows/artifacts.yml";
const SBOM_PREDICATE: &str = "--predicate-type https://cyclonedx.org/bom";
const ISSUER: &str = "https://token.actions.githubusercontent.com";
const PROVENANCE: &str = "https://slsa.dev/provenance/v1";
/// The proxy a verification with a trusted root runs under: nothing listens on it.
const DEAD_PROXY: &str = "http://127.0.0.1:9";
/// A commit the files were not built from.
const OTHER: &str = "badbad0123456789badbad0123456789badbad01";

/// An empty directory of its own for one test.
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("xtask-assets")
        .join(name);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).expect("clear scratch");
    }
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
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

/// The branch the development line is built and attested on (docs/artifacts.md).
const DEV_REF: &str = "refs/heads/feature/arch-v2";

/// What the release path leaves: `<root>/dist` packaged and summed as the workflow
/// does, `<root>/gh/gh` the fake gh, `<root>/tmp` the temporary directory.
struct Release {
    dist: PathBuf,
    gh: PathBuf,
    tmp: PathBuf,
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
        Release { dist, gh, tmp }
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

    /// The tarballs of the release, by name.
    fn tarballs(&self) -> Vec<(String, String)> {
        self.sums()
            .into_iter()
            .filter(|(_, name)| name.ends_with(".tar.gz"))
            .collect()
    }

    /// Writes next to each tarball the bundle the attest job leaves: one attestation of
    /// that tarball, signed from `source_ref` for `commit`.
    fn bundles(&self, source_ref: &str, commit: &str) {
        for (sha, name) in self.tarballs() {
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
            if name.ends_with(".tar.gz") {
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
        "xtask: SHA256SUMS lists notes.txt, neither a tarball nor an SBOM\n"
    );
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
