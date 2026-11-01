//! The offline check that a release is the one the build farm's workflow built.
//!
//! A [`GhCheck`] runs the pinned `gh` (`gh attestation verify`, a Sigstore verification
//! that needs no network when it is given the bundle and a trusted root) on a file and
//! the attestation bundle for it, and accepts the file only if the attestation
//!
//! - names the host's repository (by default `komira-ai/kbf`), signed by its workflow
//!   (by default `artifacts.yml`; a release channel of the host-install helper may name another) through the
//!   GitHub Actions OIDC issuer, on hosted runners only (`--deny-self-hosted-runners`);
//! - was made for the release's commit (`--source-digest`) on one of the host's allowed
//!   branches (`--source-ref`);
//! - is a SLSA provenance, and lists the file's sha256 as one of its subjects (what
//!   `gh attestation verify <file>` checks).
//!
//! The trust it rests on is GitHub's build attestation of the artifacts workflow on the
//! host's source ref (by default `refs/heads/main`), the pinned `gh` and the trusted root.
//! Both are root-owned files the host's bootstrap installs ([`crate::trust`]); neither the
//! frontend nor the inbox is trusted for integrity. Residuals: `main` is not
//! branch-protected, and nothing here refuses an older attested release; the ref is the
//! host's own setting ([`crate::settings`]).
//!
//! `gh` is started by absolute path with an environment of seven variables and a proxy
//! that refuses every connection (so the check is offline whatever the host's network
//! does), in a scratch home directory, as the host's unprivileged user. The file it reads
//! is the root-owned copy the stage made ([`crate::stage`]), never the inbox's.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use kbf_host::exec::{Exec, Kind};
use kbf_host::path::{HostPath, Root};

use crate::CommitId;
use crate::layout::Layout;
use crate::refusal::{Refusal, WHAT_GH};
use crate::run::{Ran, RunAs, Runner, Spawn};
use crate::settings::Settings;
use crate::trust::trusted_chain;

/// The repository whose workflow's attestation is accepted.
pub const REPO: &str = "komira-ai/kbf";
/// The workflow that builds and attests the release artifacts.
pub const SIGNER_WORKFLOW: &str = "komira-ai/kbf/.github/workflows/artifacts.yml";
/// The repository's name before the rename. A certificate carries the repository's name
/// at signing time, so releases attested before the rename name this one. Accepted for
/// one release cycle (the transition); delete this pair, [`Signer::former`] and its
/// entry in [`Verifier::pinned`] once no release under the old name is still installable.
pub const FORMER_REPO: &str = "komira-ai/komira-build-farm";
/// The workflow of [`FORMER_REPO`].
pub const FORMER_SIGNER_WORKFLOW: &str =
    "komira-ai/komira-build-farm/.github/workflows/artifacts.yml";
/// The OIDC issuer of GitHub Actions, which signs the attestation's certificate.
pub const ISSUER: &str = "https://token.actions.githubusercontent.com";
/// The kind of attestation `gh` is asked for: SLSA build provenance.
pub const PREDICATE: &str = "https://slsa.dev/provenance/v1";
/// A proxy that refuses every connection (the discard port on loopback).
pub const DEAD_PROXY: &str = "http://127.0.0.1:9";

/// A branch ref a release may be attested for: `refs/heads/` and a branch name of
/// letters, digits and `. _ - /`. It is passed to `gh` as an argument, so what cannot be
/// one (a space, a leading `-`, `..`) is refused here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceRef(String);

/// Why a string is not a [`SourceRef`].
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0:?} is not a branch ref (refs/heads/ and a name of letters, digits and . _ - /)")]
pub struct NotARef(pub String);

impl SourceRef {
    pub fn new(name: &str) -> Result<SourceRef, NotARef> {
        let branch = name.strip_prefix("refs/heads/").unwrap_or("");
        let plain = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/');
        let sound = !branch.is_empty()
            && branch.chars().all(plain)
            && !branch.starts_with('/')
            && !branch.ends_with('/')
            && !branch.contains("//")
            && !branch.contains("..");
        if sound {
            Ok(SourceRef(name.to_owned()))
        } else {
            Err(NotARef(name.to_owned()))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// What the check established about a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verified {
    /// The allowed ref the attestation was made for.
    pub source_ref: SourceRef,
}

/// One repository and workflow whose attestation is accepted. `gh` is asked for both in
/// one call, so an attestation must name this pair, never the repository of one and the
/// workflow of another.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signer {
    pub repo: String,
    pub workflow: String,
}

impl Signer {
    /// The repository under its current name.
    pub fn current() -> Signer {
        Signer {
            repo: REPO.to_owned(),
            workflow: SIGNER_WORKFLOW.to_owned(),
        }
    }

    /// The repository under its name before the rename (see [`FORMER_REPO`]).
    pub fn former() -> Signer {
        Signer {
            repo: FORMER_REPO.to_owned(),
            workflow: FORMER_SIGNER_WORKFLOW.to_owned(),
        }
    }
}

/// What `gh attestation verify` is asked, and how it is run.
#[derive(Clone, Debug)]
pub struct GhCheck {
    /// The directory the paths below are under (`/` on a host).
    pub root: Root,
    /// The pinned `gh`.
    pub gh: HostPath,
    /// The Sigstore trusted root, as `gh attestation trusted-root` prints it.
    pub trusted_root: HostPath,
    /// The signers an attestation may name, tried in order.
    pub signers: Vec<Signer>,
    pub issuer: String,
    /// The branches the attestation may name, tried in order.
    pub source_refs: Vec<SourceRef>,
    /// The user `gh` runs as.
    pub run_as: RunAs,
    pub timeout: Duration,
}

/// The things a verification asks of the host: who owns what, and running a program.
pub struct Tools<'a> {
    pub trust: &'a dyn Exec,
    pub runner: &'a dyn Runner,
}

impl GhCheck {
    /// The verifier of a host: the pinned repository, workflow and issuer, with the
    /// host's settings.
    pub fn pinned(host: &Layout, settings: &Settings) -> GhCheck {
        GhCheck {
            root: host.root().clone(),
            gh: host.gh(),
            trusted_root: host.trusted_root(),
            signers: settings.signers.clone(),
            issuer: ISSUER.to_owned(),
            source_refs: settings.source_refs.clone(),
            run_as: settings.run_as,
            timeout: settings.timeout,
        }
    }

    /// Whether `gh` and the trusted root may be believed (see [`crate::trust`]).
    pub fn check_trust(&self, trust: &dyn Exec) -> Result<(), Refusal> {
        trusted_chain(trust, &self.gh, Kind::File)?;
        trusted_chain(trust, &self.trusted_root, Kind::File)
    }

    /// The argument list of `gh` for `tarball` and its `bundle`, for one signer and one
    /// allowed ref:
    ///
    /// ```text
    /// attestation verify <tarball> --bundle <bundle> --custom-trusted-root <root>
    ///   --repo R --signer-workflow S --cert-oidc-issuer I --source-ref REF
    ///   --source-digest <commit> --predicate-type P --deny-self-hosted-runners
    /// ```
    pub fn args(
        &self,
        tarball: &Path,
        bundle: &Path,
        commit: &CommitId,
        signer: &Signer,
        source_ref: &SourceRef,
    ) -> Vec<OsString> {
        let mut args: Vec<OsString> = ["attestation", "verify"].map(OsString::from).to_vec();
        args.push(tarball.into());
        args.push("--bundle".into());
        args.push(bundle.into());
        args.push("--custom-trusted-root".into());
        args.push(self.root.join(&self.trusted_root).into());
        for (flag, value) in [
            ("--repo", signer.repo.as_str()),
            ("--signer-workflow", signer.workflow.as_str()),
            ("--cert-oidc-issuer", self.issuer.as_str()),
            ("--source-ref", source_ref.as_str()),
            ("--source-digest", commit.as_str()),
            ("--predicate-type", PREDICATE),
        ] {
            args.push(flag.into());
            args.push(value.into());
        }
        args.push("--deny-self-hosted-runners".into());
        args
    }

    /// The whole environment `gh` gets: a fixed `PATH`, a scratch home so it writes
    /// nowhere else, no prompts or update notices, and a proxy that refuses every
    /// connection. Nothing of the caller's environment (a token included) is in it.
    pub fn env(&self, home: &Path) -> Vec<(String, String)> {
        let home = home.display().to_string();
        let config = format!("{home}/config");
        let mut env = vec![
            ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
            ("HOME".to_owned(), home),
            ("GH_CONFIG_DIR".to_owned(), config),
            ("GH_NO_UPDATE_NOTIFIER".to_owned(), "1".to_owned()),
            ("GH_PROMPT_DISABLED".to_owned(), "1".to_owned()),
            ("NO_COLOR".to_owned(), "1".to_owned()),
        ];
        for proxy in ["HTTPS_PROXY", "HTTP_PROXY", "ALL_PROXY"] {
            env.push((proxy.to_owned(), DEAD_PROXY.to_owned()));
        }
        env
    }

    /// Accept `tarball` only if `gh` finds `bundle` a valid attestation of it for
    /// `commit` on one of the allowed refs, from one of the accepted signers. `home` is a directory `run_as` owns, which
    /// `gh` runs in. `tarball` and `bundle` are the stage's own copies.
    ///
    /// A `gh` that exits non-zero for every signer and ref is [`Refusal::NotAttested`]; one that
    /// cannot be started, or is killed at the timeout, is its own refusal. A trust list
    /// that is not root's alone is refused first, naming the path ([`Refusal::Untrusted`]).
    pub fn verify(
        &self,
        tools: &Tools,
        home: &Path,
        tarball: &Path,
        bundle: &Path,
        commit: &CommitId,
    ) -> Result<Verified, Refusal> {
        self.check_trust(tools.trust)?;
        let subject = tarball
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let mut last = None;
        for signer in &self.signers {
            for source_ref in &self.source_refs {
                let ran = self.run_gh(
                    tools.runner,
                    home,
                    (tarball, bundle),
                    commit,
                    (signer, source_ref),
                )?;
                if ran.code == Some(0) {
                    return Ok(Verified {
                        source_ref: source_ref.clone(),
                    });
                }
                last = Some(ran);
            }
        }
        Err(Refusal::NotAttested {
            subject,
            detail: complaint(last.as_ref()),
        })
    }

    fn run_gh(
        &self,
        runner: &dyn Runner,
        home: &Path,
        (tarball, bundle): (&Path, &Path),
        commit: &CommitId,
        (signer, source_ref): (&Signer, &SourceRef),
    ) -> Result<Ran, Refusal> {
        let spawn = Spawn {
            program: self.root.join(&self.gh),
            args: self.args(tarball, bundle, commit, signer, source_ref),
            env: self.env(home),
            cwd: PathBuf::from(home),
            run_as: Some(self.run_as),
            timeout: self.timeout,
        };
        let ran = runner.run(&spawn).map_err(|source| Refusal::Tool {
            what: WHAT_GH,
            source,
        })?;
        if ran.timed_out {
            return Err(Refusal::TimedOut { what: WHAT_GH });
        }
        Ok(ran)
    }
}

/// The first line `gh` printed to stderr, cut to 160 characters with anything but
/// printable ASCII replaced: for the operator, not for a decision.
fn complaint(ran: Option<&Ran>) -> String {
    let text = ran.map_or_else(String::new, |r| {
        String::from_utf8_lossy(&r.stderr).into_owned()
    });
    let line = text.lines().next().unwrap_or("");
    line.chars()
        .take(160)
        .map(|c| {
            if c.is_ascii_graphic() || c == ' ' {
                c
            } else {
                '?'
            }
        })
        .collect()
}

#[cfg(test)]
#[path = "attest_tests.rs"]
mod tests;
