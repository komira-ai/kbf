//! The real [`Fetcher`]: what the cycle calls for an `activate` or a `frontend_update` order.
//! It joins the P7 fetch child to the P8 verify pipeline, in the order of
//! `docs/design/kbf-installer.md` section 5 and changes nothing but scratch until the pipeline
//! has accepted every file:
//!
//! 1. `installer.toml` (root's alone, like the trust files) says where bytes come from and what
//!    bounds a fetch. An order, an offer and the environment say nothing about either.
//! 2. An `activate` order names its components and the facts it expects. A `frontend_update`
//!    names neither (the proxy that writes it has no release store): the components of the
//!    commit are found in the source's index, `index/<commit>/<platform>/kbf` (`<sha256> <size>`)
//!    and `<that>.attestation`, read through the same confined child. Those are claims like
//!    any locator: the tarball's digest and size are checked when it arrives and the attestation
//!    decides whether it is believed. Only the `url` source has an index.
//! 3. Each tarball is pulled by the child into `work/<commit>.partial/dl/`, a file this parent
//!    created, and hashed as it arrives ([`super::fetch_component`]).
//! 4. [`Pipeline`] counts, hashes, attests, unpacks, probes and installs it as
//!    `releases/<commit>/` and records it as staged. A refusal at any step leaves nothing in
//!    `releases/`; the scratch directories go whatever the outcome.
//!
//! A refusal reaches the cycle as a [`FetchError`] whose text begins with the contract's stable
//! word (`hash`, `attestation`, `size`, `disk`, ...), or is `fetch` itself when the source could
//! not be reached.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use kbf_host::exec::{Entry, Exec, Kind};
use kbf_host::path::HostPath;
use kbf_host::step::Owner;
use kbf_update::Locator;

use super::{
    ATTESTATION_SUFFIX, ChildProgram, Component, Limits, Source, fetch_component, fetch_index,
    is_digest,
};
use crate::CommitId;
use crate::helper::fetcher::{self, Fetcher, InstalledRelease, ReleaseLocator};
use crate::helper::leases::LeaseSource;
use crate::installer_toml::{self, InstallerToml};
use crate::layout::{Layout, Platform, path};
use crate::refusal::Refusal;
use crate::run::Runner;
use crate::settings::Settings;
use crate::stage::{
    BUNDLE_CAP, Fetched, MAX_BINARY, Order, OrderKind, Partial, Pipeline, Policy, Received,
    Supported, Timeouts, Verifier, platform_of,
};
use crate::trust::trusted_chain;

/// The one component every platform's release carries.
const COMPONENT: &str = "kbf";
/// The most an index entry naming a component (`<sha256> <size>`) may be.
const INDEX_LINE_MAX: u64 = 256;

/// Where `installer.toml` is on a host of `platform`.
pub fn default_config(platform: Platform) -> HostPath {
    path(match platform {
        Platform::Linux => "/etc/kbf/installer.toml",
        Platform::MacOs => "/usr/local/etc/kbf/installer.toml",
    })
}

/// What a release may declare and still be one this installer can act on. The installer does
/// not speak the worker protocol (the frontend's compatibility gate orders those), so it sets
/// no upper bound on `protocol` and `min_protocol`; the state schema is the newest migration it
/// embeds, since a release that needs a newer one needs a newer installer first.
pub fn supported() -> Supported {
    Supported {
        protocol: u64::MAX,
        min_protocol: u64::MAX,
        state_schema: u64::from(kbf_state::postgres::schema::latest()),
    }
}

/// Pulls a release through the fetch child, verifies it and installs it.
pub struct ReleaseFetcher {
    host: Layout,
    /// `installer.toml`, read afresh at every fetch.
    config: HostPath,
    /// The installer's own binary: the program the child is started from.
    program: PathBuf,
    /// Answers who owns what, and reads the root-owned files.
    trust: Box<dyn Exec>,
    runner: Box<dyn Runner>,
    leases: Box<dyn LeaseSource>,
    /// Who owns the release: root on a host.
    owner: Owner,
    supported: Supported,
    timeouts: Timeouts,
    /// Roots to trust for the url source instead of the system's. Not a setting of
    /// `installer.toml`; a test's server runs under its own CA.
    roots: Option<PathBuf>,
}

impl ReleaseFetcher {
    /// The fetcher of a host: `program` is this installer's own file, `config` its
    /// `installer.toml`. The release is owned by root.
    pub fn new(
        host: Layout,
        config: HostPath,
        program: PathBuf,
        trust: Box<dyn Exec>,
        runner: Box<dyn Runner>,
        leases: Box<dyn LeaseSource>,
    ) -> ReleaseFetcher {
        ReleaseFetcher {
            host,
            config,
            program,
            trust,
            runner,
            leases,
            owner: Owner { uid: 0, gid: 0 },
            supported: supported(),
            timeouts: Timeouts::DEFAULT,
            roots: None,
        }
    }

    /// Own the release as `owner` (a scratch test cannot chown to root).
    #[cfg(feature = "test-sandbox")]
    #[must_use]
    pub fn owned_by(mut self, owner: Owner) -> ReleaseFetcher {
        self.owner = owner;
        self
    }

    /// Trust the roots in `file` for the url source instead of the system's.
    #[cfg(feature = "test-sandbox")]
    #[must_use]
    pub fn trusting(mut self, file: PathBuf) -> ReleaseFetcher {
        self.roots = Some(file);
        self
    }

    fn run(&self, rec: &ReleaseLocator) -> Result<InstalledRelease, Fault> {
        let kind = match (rec.components.is_empty(), &rec.expected) {
            (true, None) => OrderKind::FrontendUpdate,
            (false, Some(_)) => OrderKind::Activate,
            _ => {
                return Err(Fault::Config(
                    "a release locator names components and expected facts, or neither".to_owned(),
                ));
            }
        };
        let config = self.read_config()?;
        let mut settings = Settings::load(&*self.trust, &self.host)?;
        narrow(&mut settings, &config)?;
        let source = match &config.source {
            installer_toml::Source::Url(base) => Source::Url {
                base: base.clone(),
                ca_file: self.roots.clone(),
            },
            installer_toml::Source::Frontend => {
                return Err(Fault::Config(
                    "release_source = \"frontend\" is not wired: installer.toml names neither \
                     the frontend's address nor the pinned CA"
                        .to_owned(),
                ));
            }
        };
        let limits = Limits {
            max_component_bytes: config.max_component_bytes,
            fetch_timeout: Duration::from_secs(u64::from(config.fetch_timeout_s)),
        };
        let child = ChildProgram {
            program: self.program.clone(),
            run_as: Some(settings.run_as),
        };
        sweep_stale(&self.host, &rec.commit);
        let partial = Partial::create(&self.host, &rec.commit, self.owner)?;

        let locators = match kind {
            OrderKind::Activate => rec.components.clone(),
            OrderKind::FrontendUpdate => {
                let platform = platform_of(self.host.platform(), settings.arch);
                vec![self.find_component(&child, &source, limits, &partial, &rec.commit, &platform)?]
            }
        };
        let mut files = Vec::new();
        for locator in &locators {
            files.push(self.pull(&child, &source, limits, &partial, locator)?);
        }

        let pipeline = Pipeline {
            host: &self.host,
            settings: &settings,
            policy: Policy {
                max_component_bytes: config.max_component_bytes,
                max_binary_bytes: MAX_BINARY,
                min_protocol: u64::from(config.min_protocol),
                supported: self.supported,
                timeouts: self.timeouts,
            },
            trust: &*self.trust,
            runner: &*self.runner,
            leases: &*self.leases,
            owner: self.owner,
        };
        let order = Order {
            kind,
            commit: &rec.commit,
            components: &locators,
            expected: rec.expected.as_ref(),
        };
        let staged = pipeline.verify(&order, Fetched { partial, files })?;
        Ok(staged.installed())
    }

    /// `installer.toml`, which must be root's alone, as the trust files are.
    fn read_config(&self) -> Result<InstallerToml, Fault> {
        let at = &self.config;
        let found = self
            .trust
            .inspect(at)
            .map_err(|e| Fault::Config(format!("{}: {e}", at.as_str())))?;
        let Some(Entry::File { bytes, .. }) = found else {
            return Err(Fault::Config(format!(
                "{} is missing or is not a plain file",
                at.as_str()
            )));
        };
        trusted_chain(&*self.trust, at, Kind::File)?;
        let text = String::from_utf8(bytes)
            .map_err(|_| Fault::Config(format!("{} is not UTF-8", at.as_str())))?;
        InstallerToml::parse(&text).map_err(|e| Fault::Config(e.to_string()))
    }

    /// The tarball and attestation of `kbf` for `commit` on `platform`, as the url source's
    /// index says them.
    fn find_component(
        &self,
        child: &ChildProgram,
        source: &Source,
        limits: Limits,
        partial: &Partial,
        commit: &CommitId,
        platform: &str,
    ) -> Result<Locator, Fault> {
        let entry = format!("{commit}/{platform}/{COMPONENT}");
        let line = self.read_index(child, source, limits, partial, &entry, INDEX_LINE_MAX)?;
        let (sha256, size) = parse_index_line(&line)
            .map_err(|why| Fault::Config(format!("index entry {entry}: {why}")))?;
        let bundle = self.read_index(
            child,
            source,
            limits,
            partial,
            &format!("{entry}{ATTESTATION_SUFFIX}"),
            BUNDLE_CAP,
        )?;
        let attestation = String::from_utf8(bundle)
            .map_err(|_| Fault::Config(format!("index entry {entry}: the bundle is not UTF-8")))?;
        Ok(Locator {
            name: COMPONENT.to_owned(),
            sha256,
            size,
            attestation,
        })
    }

    fn read_index(
        &self,
        child: &ChildProgram,
        source: &Source,
        limits: Limits,
        partial: &Partial,
        entry: &str,
        max: u64,
    ) -> Result<Vec<u8>, Fault> {
        let file = partial.dl().join(entry.rsplit('/').next().unwrap_or("index"));
        let mut out = scratch_file(&file)?;
        fetch_index(child, source, limits, entry, max, &mut out)?;
        drop(out);
        Ok(fs::read(&file)?)
    }

    /// One component's tarball, into the download directory.
    fn pull(
        &self,
        child: &ChildProgram,
        source: &Source,
        limits: Limits,
        partial: &Partial,
        locator: &Locator,
    ) -> Result<Received, Fault> {
        let plain = !locator.name.is_empty()
            && locator
                .name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
        if !plain {
            return Err(Fault::Config(format!(
                "{:?} is not a component name",
                locator.name
            )));
        }
        let tarball = partial.dl().join(format!("{}.tar.gz", locator.name));
        let mut out = scratch_file(&tarball)?;
        // The release this one replaces is still there until the swap is confirmed.
        let replaced = fs::metadata(self.host.join(&self.host.current()).join(COMPONENT))
            .map_or(0, |m| m.len());
        let got = fetch_component(
            child,
            source,
            limits,
            &Component {
                sha256: &locator.sha256,
                size: locator.size,
            },
            (partial.path(), replaced),
            &mut out,
        )?;
        Ok(Received {
            name: locator.name.clone(),
            tarball,
            bytes: got.count,
            sha256: got.sha256,
        })
    }
}

impl Fetcher for ReleaseFetcher {
    fn fetch(&self, rec: &ReleaseLocator) -> Result<InstalledRelease, fetcher::FetchError> {
        self.run(rec).map_err(Fault::into_error)
    }
}

/// A new file in the download directory, which only this parent can write.
fn scratch_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

/// `installer.toml` may narrow the trust files' refs and never widen them.
fn narrow(settings: &mut Settings, config: &InstallerToml) -> Result<(), Fault> {
    if config.source_refs.is_empty() {
        return Ok(());
    }
    settings
        .source_refs
        .retain(|r| config.source_refs.iter().any(|c| c == r.as_str()));
    if settings.source_refs.is_empty() {
        return Err(Fault::Config(
            "source_refs of installer.toml leave none of the refs the trust files allow"
                .to_owned(),
        ));
    }
    Ok(())
}

/// A `work/<commit>.partial/` that is still there belongs to a pass that died: nothing else
/// runs a fetch of this host, so it is removed and the fetch starts clean.
fn sweep_stale(host: &Layout, commit: &CommitId) {
    let stale = host.join(&host.work_dir()).join(format!("{commit}.partial"));
    let _ = fs::remove_dir_all(stale);
}

/// `<sha256> <size>\n`, and nothing else.
pub(crate) fn parse_index_line(bytes: &[u8]) -> Result<(String, u64), String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "not UTF-8".to_owned())?;
    let line = text
        .strip_suffix('\n')
        .ok_or("no line end".to_owned())?;
    let (digest, size) = line
        .split_once(' ')
        .ok_or("not \"<sha256> <size>\"".to_owned())?;
    if !is_digest(digest) {
        return Err(format!("{digest:?} is not a sha256"));
    }
    if size.is_empty() || !size.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("{size:?} is not a size"));
    }
    let size = size.parse().map_err(|_| format!("{size:?} is not a size"))?;
    Ok((digest.to_owned(), size))
}

/// Why a release was not installed, before it is told to the cycle.
#[derive(Debug, thiserror::Error)]
enum Fault {
    #[error("{0}")]
    Config(String),
    #[error("{0}")]
    Fetch(#[from] super::FetchError),
    #[error("{0}")]
    Refused(#[from] Refusal),
    #[error("{0}")]
    Work(#[from] io::Error),
}

impl Fault {
    /// The cycle's error: `fetch` when the release could not be had, and otherwise a rejection
    /// that begins with the stable word of the contract.
    fn into_error(self) -> fetcher::FetchError {
        let rejected = |word: &str, why: &dyn std::fmt::Display| {
            fetcher::FetchError::Rejected(format!("{word}: {why}"))
        };
        match self {
            Fault::Fetch(e) if e.word() != "fetch" => rejected(e.word(), &e),
            Fault::Refused(r) => match r.word() {
                Some(word) => rejected(word, &r),
                None => fetcher::FetchError::Unavailable(r.to_string()),
            },
            other => fetcher::FetchError::Unavailable(other.to_string()),
        }
    }
}
