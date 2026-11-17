//! The release path of `assets`: the packaged and summed `dist`, the fake gh and its store.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use kbf_testkit::dir::TestDir;
use sha2::Digest;

pub(crate) const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
pub(crate) const FAKE: &str = env!("CARGO_BIN_EXE_xtask-fake");
pub(crate) const COMMIT: &str = "c0ffee0123456789c0ffee0123456789c0ffee01";
pub(crate) const REPO: &str = "komira-ai/kbf";
pub(crate) const WORKFLOW: &str = "komira-ai/kbf/.github/workflows/artifacts.yml";
pub(crate) const SBOM_PREDICATE: &str = "--predicate-type https://cyclonedx.org/bom";
pub(crate) const ISSUER: &str = "https://token.actions.githubusercontent.com";
pub(crate) const PROVENANCE: &str = "https://slsa.dev/provenance/v1";
/// The proxy a verification with a trusted root runs under: nothing listens on it.
pub(crate) const DEAD_PROXY: &str = "http://127.0.0.1:9";
/// A commit the files were not built from.
pub(crate) const OTHER: &str = "badbad0123456789badbad0123456789badbad01";

/// An empty directory of its own for one test.
pub(crate) fn scratch(name: &str) -> TestDir {
    TestDir::under(
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("xtask-assets"),
        name,
    )
}

/// The exit code, stdout and stderr of `cargo xtask <args>`.
pub(crate) fn xtask(
    args: &[&str],
    tmp: &Path,
    gh_defect: Option<&str>,
) -> (Option<i32>, String, String) {
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

pub(crate) fn s(p: &Path) -> &str {
    p.to_str().expect("utf-8 path")
}

/// The flags a deployment verifies an asset with (docs/artifacts.md), signed by
/// `signer`, from the branch `source_ref`.
pub(crate) fn flags_from(signer: &str, source_ref: &str) -> String {
    format!(
        "--repo {REPO} --signer-workflow {signer} --source-ref {source_ref} \
         --deny-self-hosted-runners"
    )
}

/// [`flags_from`] `refs/heads/main`.
pub(crate) fn flags(signer: &str) -> String {
    flags_from(signer, "refs/heads/main")
}

/// The flags a host's installer verifies a tarball with against its bundle, signed by
/// `artifacts.yml` from the branch `source_ref` for `commit` (the `--bundle` and
/// `--custom-trusted-root` words name files and are not part of it).
pub(crate) fn bundle_flags(source_ref: &str, commit: &str) -> String {
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
pub(crate) fn bundle_json(sha: &str, flags: &str) -> String {
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
pub(crate) const DEV_REF: &str = "refs/heads/feature/arch-v2";

/// What the release path leaves: `<root>/dist` packaged and summed as the workflow
/// does, `<root>/gh/gh` the fake gh, `<root>/tmp` the temporary directory.
pub(crate) struct Release {
    pub(crate) dist: PathBuf,
    pub(crate) gh: PathBuf,
    pub(crate) tmp: PathBuf,
    /// The directory all of it is in; last, so it is removed after the rest.
    pub(crate) _root: TestDir,
}

impl Release {
    /// Packages kbf-deploy for every platform and kbf-server for Linux, then
    /// assembles SHA256SUMS, each through the CLI as the workflow runs it.
    pub(crate) fn build(name: &str) -> Release {
        Release::build_with(
            name,
            &[
                ("darwin-arm64", &["kbf-deploy"][..]),
                ("linux-x86_64", &["kbf-deploy", "kbf-server"][..]),
                ("linux-arm64", &["kbf-deploy", "kbf-server"][..]),
            ],
        )
    }

    /// [`Release::build`] of the binaries `jobs` name for each platform.
    pub(crate) fn build_with(name: &str, jobs: &[(&str, &[&str])]) -> Release {
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
    pub(crate) fn sums(&self) -> Vec<(String, String)> {
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
    pub(crate) fn attest(&self, skip: Option<(&str, bool)>) {
        self.attest_from(skip, "refs/heads/main");
    }

    /// The files with a bundle: the tarballs, the `.deb` packages and the image tarballs.
    pub(crate) fn bundled(&self) -> Vec<(String, String)> {
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
    pub(crate) fn has_image_sbom(&self, name: &str) -> bool {
        name.strip_suffix(".oci.tar").is_some_and(|stem| {
            let sbom = format!("{stem}.cdx.json");
            self.sums().iter().any(|(_, n)| *n == sbom)
        })
    }

    /// Adds a deb of `platform` as `pkg-kbf-asset` leaves it: the file in
    /// `dist` (distinct bytes: an attestation covers a digest), listed in `SHA256SUMS`
    /// (sorted by name, as `sums assemble` leaves it).
    pub(crate) fn add_deb(&self, platform: &str) -> String {
        self.add_package(platform, "deb")
    }

    /// Adds the `kbf-installer` package of `platform` with the extension `ext` (`deb`, or
    /// `pkg` for the Mac's), as [`Release::add_deb`] does.
    pub(crate) fn add_package(&self, platform: &str, ext: &str) -> String {
        let name = format!("kbf-installer-{COMMIT}-{platform}.{ext}");
        let bytes = format!("the kbf-installer package for {platform}");
        self.add_listed(&name, &bytes);
        name
    }

    /// Adds the version facts of `kbf` for `platform` as `release version-facts` leaves
    /// them: a file in `dist` (distinct bytes), listed in `SHA256SUMS`.
    pub(crate) fn add_facts(&self, platform: &str) -> String {
        let name = format!("kbf-{COMMIT}-{platform}.version.json");
        let bytes = format!("the version facts of kbf for {platform}\n");
        self.add_listed(&name, &bytes);
        name
    }

    /// Adds the file `name` holding `bytes` to `dist` and to `SHA256SUMS`, which stays
    /// sorted by name as `sums assemble` leaves it.
    pub(crate) fn add_listed(&self, name: &str, bytes: &str) {
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
    pub(crate) fn bundles(&self, source_ref: &str, commit: &str) {
        for (sha, name) in self.bundled() {
            std::fs::write(
                self.dist.join(format!("{name}.sigstore.json")),
                bundle_json(&sha, &bundle_flags(source_ref, commit)),
            )
            .expect("bundle");
        }
    }

    /// [`Release::attest`] for a run on the branch `source_ref`.
    pub(crate) fn attest_from(&self, skip: Option<(&str, bool)>, source_ref: &str) {
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

    /// Removes from the fake's store the provenance attestations of the files `names`, so
    /// each of them fails its check (after [`Release::attest`]).
    pub(crate) fn unattest(&self, names: &[&str]) {
        let gone: Vec<String> = self
            .sums()
            .into_iter()
            .filter(|(_, n)| names.contains(&n.as_str()))
            .map(|(sha, _)| format!("{sha} "))
            .collect();
        let store = self.gh.with_file_name("attested");
        let kept: String = std::fs::read_to_string(&store)
            .expect("store")
            .lines()
            .filter(|l| !gone.iter().any(|g| l.starts_with(g.as_str())))
            .map(|l| format!("{l}\n"))
            .collect();
        std::fs::write(store, kept).expect("store");
    }

    pub(crate) fn verify(
        &self,
        workflow_ref: &str,
        defect: Option<&str>,
    ) -> (Option<i32>, String, String) {
        self.verify_args(workflow_ref, &[], defect)
    }

    /// [`Release::verify`] with `more` flags.
    pub(crate) fn verify_args(
        &self,
        workflow_ref: &str,
        more: &[&str],
        defect: Option<&str>,
    ) -> (Option<i32>, String, String) {
        self.verify_commit(workflow_ref, COMMIT, more, defect)
    }

    /// [`Release::verify_args`] for the commit `commit`.
    pub(crate) fn verify_commit(
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

    pub(crate) fn gh_calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.gh.with_file_name("gh-args"))
            .expect("gh-args")
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// The first `n` of `calls` (the per-file checks, which run concurrently and so land
    /// in the log in any order) put in the order of `SHA256SUMS`, each file's own calls
    /// staying in the order made; the rest (the serial controls) as they are.
    pub(crate) fn per_file_in_sums_order(&self, mut calls: Vec<String>, n: usize) -> Vec<String> {
        let names: Vec<String> = self
            .sums()
            .into_iter()
            .map(|(_, name)| s(&self.dist.join(name)).to_owned())
            .collect();
        let at = |call: &String| {
            let file = call.split(' ').nth(2).unwrap_or_default();
            names.iter().position(|name| name == file)
        };
        calls[..n].sort_by_key(at);
        calls
    }

    /// The `gh attestation verify` calls made with a bundle.
    pub(crate) fn bundle_calls(&self) -> Vec<String> {
        self.gh_calls()
            .into_iter()
            .filter(|c| c.contains(" --bundle "))
            .collect()
    }

    /// The proxy and no-proxy variables of each call made with a bundle, one line each
    /// (`NAME=value`, empty when unset).
    pub(crate) fn bundle_proxies(&self) -> Vec<String> {
        std::fs::read_to_string(self.gh.with_file_name("gh-env"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// The path of the bundle of the tarball `name`.
    pub(crate) fn bundle_of(&self, name: &str) -> PathBuf {
        self.dist.join(format!("{name}.sigstore.json"))
    }
}

pub(crate) fn first_tarball() -> String {
    format!("kbf-deploy-{COMMIT}-darwin-arm64.tar.gz")
}

/// The second tarball in `SHA256SUMS` order, the one the cross-bundle control uses.
pub(crate) fn second_tarball() -> String {
    format!("kbf-deploy-{COMMIT}-linux-arm64.tar.gz")
}

/// `commit` with its last digit changed, as `verify-assets` makes the control commit.
pub(crate) fn control_commit(commit: &str) -> String {
    let (head, last) = commit.split_at(commit.len() - 1);
    format!("{head}{}", if last == "0" { "1" } else { "0" })
}
