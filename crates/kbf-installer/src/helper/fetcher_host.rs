//! The release fetcher of the binary: a [`RealFetcher`] made anew for each fetch, from what the
//! host says at that moment.
//!
//! Nothing is read when the fetcher is built, and nothing is kept between two fetches. That is
//! deliberate, for two reasons:
//!
//! - `setup` builds its fetcher before it has written `installer.toml` or made the `kbf-fetch`
//!   user, and both must be there when the first release is fetched;
//! - `run` is a long-lived service: a second run of `setup` (a new source, a new floor) must
//!   take effect at the next order without a restart, and so must a changed `installer.conf`.
//!
//! What a fetch uses, and where each part comes from:
//!
//! - `installer.toml` ([`InstallerToml::read`]): the source, the component cap and the fetch
//!   timeout, the operator's `min_protocol` floor ([`Policy::of`], [`Limits::of`]), and the
//!   refs it names, which can only narrow `installer.conf`'s list
//!   ([`Settings::narrowed_to`]). The file is held to the trust files' rule: root's alone.
//! - `installer.conf` ([`Settings::load`]), for the refs a release may be attested for and the
//!   unprivileged user `gh` and the probe run as.
//! - the host ([`Parts`]): the executor that answers for ownership, the runner, the lease
//!   source, the owner of the scratch directory and the fetch child's program.
//!
//! Any of them that cannot be had makes the fetch `Unavailable` with the reason, so the cycle
//! refuses the order (`fetch`) and goes on, and `setup` says why it stopped.
//!
//! An order says whether it wants the release fetched from the configured source or the one
//! the installer's package carries ([`ReleaseLocator::source`]), and this is the one place that
//! decides it against the configuration, so a caller that did not look, or looked at a file the
//! next read no longer says, cannot get a fetch the host does not allow:
//!
//! - a **fetch** needs a source that fetches. Under `release_source = "bundled"` it is denied
//!   (`denied`), by the fetcher, before the fetch child is asked for anything;
//! - a **staged** order takes the package's release on every kind of host, a `bundled` one and
//!   one with a source of its own alike, and starts no fetch child: the seed fetcher
//!   ([`SeedFetcher`]) hands the package's files to the same verify pipeline.

use kbf_host::{Entry, Exec, HostExec, HostPath, Kind, Owner};
use kbf_update::{Facts, Source as OrderSource};
use sha2::{Digest, Sha256};

use crate::CommitId;
use crate::binary::Version;
use crate::fetch::{ChildProgram, FrontendSource, Limits, ProgramFault, Source};
use crate::helper::fetcher::{FetchError, Fetcher, InstalledRelease, ReleaseLocator};
use crate::helper::fetcher_real::{Parts, RealFetcher};
use crate::helper::fetcher_seed::SeedFetcher;
use crate::helper::leases::LeaseSource;
use crate::installer_toml::{InstallerToml, ReadError, Source as Configured};
use crate::layout::{Layout, Platform};
use crate::refusal::Refusal;
use crate::run::SystemRunner;
use crate::seed::{Looked, Noticed, SeedFacts};
use crate::settings::{NoCommonRef, Settings};
use crate::stage::Policy;
use crate::trust::trusted_chain;
use crate::version;

/// Pulls and installs a release the way the host is configured to, as of the moment it is
/// asked. See the module documentation.
pub struct HostFetcher {
    host: Layout,
    /// `installer.toml`, as a path under the host's root.
    installer_toml: HostPath,
    /// What the host supplies, made for each fetch.
    parts: Box<dyn Fn() -> Result<Parts, String>>,
    /// The first 12 hex digits of the commit this installer was built from: the name of the
    /// seed directory it reads.
    build: String,
}

impl HostFetcher {
    /// A fetcher on `host` that reads `installer_toml` and takes its [`Parts`] from `parts`
    /// (made for each fetch; its error is why this host cannot fetch). [`HostFetcher::on_host`]
    /// is the host's own; a test plays the host with this.
    pub fn new(
        host: Layout,
        installer_toml: HostPath,
        parts: impl Fn() -> Result<Parts, String> + 'static,
    ) -> HostFetcher {
        HostFetcher {
            host,
            installer_toml,
            parts: Box::new(parts),
            build: version::BUILD_COMMIT.to_owned(),
        }
    }

    /// The same fetcher for an installer built from the commit whose first 12 hex digits are
    /// `build` (the name of the seed directory it reads). A fetcher is made for the program
    /// that runs; a test is not built from a commit that has a seed.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_build(mut self, build: &str) -> HostFetcher {
        build.clone_into(&mut self.build);
        self
    }

    /// The fetcher of the host `host` itself: the real executor and runner, the owner of this
    /// process, the fetch child that is this program run as the fetch user, and the lease
    /// source `leases` makes.
    pub fn on_host(
        host: Layout,
        installer_toml: HostPath,
        leases: impl Fn() -> Box<dyn LeaseSource> + 'static,
    ) -> HostFetcher {
        HostFetcher::on_host_with(host, installer_toml, leases, ChildProgram::of_this_program)
    }

    /// [`HostFetcher::on_host`] with the fetch child's program made by `child` (a test cannot
    /// make the fetch user).
    fn on_host_with(
        host: Layout,
        installer_toml: HostPath,
        leases: impl Fn() -> Box<dyn LeaseSource> + 'static,
        child: fn(Platform) -> Result<ChildProgram, ProgramFault>,
    ) -> HostFetcher {
        let own = host.clone();
        HostFetcher::new(host, installer_toml, move || {
            let child = child(own.platform()).map_err(|e| e.to_string())?;
            Ok(host_parts(&own, leases(), child))
        })
    }

    /// The real fetcher for this fetch.
    fn build(&self) -> Result<RealFetcher, FetchError> {
        self.assemble().map_err(FetchError::from)
    }

    /// The seed fetcher for this fetch.
    fn build_seed(&self) -> Result<SeedFetcher, FetchError> {
        self.assemble_seed()
            .map(|(seed, _)| seed)
            .map_err(FetchError::from)
    }

    /// What either kind of fetch reads of the host, in this order: its parts, `installer.toml`
    /// and the settings it narrows.
    fn read(&self) -> Result<(Parts, InstallerToml, Settings), NotBuilt> {
        let parts = (self.parts)().map_err(NotBuilt::Host)?;
        let toml = InstallerToml::read(&*parts.trust, &self.installer_toml)?;
        let settings = Settings::load(&*parts.trust, &self.host)
            .map_err(NotBuilt::Settings)?
            .narrowed_to(&toml.source_refs)?;
        Ok((parts, toml, settings))
    }

    /// The fetcher of an order that fetches: the one place the configured source is decided,
    /// and where `bundled` is denied.
    fn assemble(&self) -> Result<RealFetcher, NotBuilt> {
        let (parts, toml, settings) = self.read()?;
        let source = source_of(&toml, &*parts.trust)?;
        Ok(RealFetcher::new(
            self.host.clone(),
            settings,
            Policy::of(&toml),
            source,
            Limits::of(&toml),
            parts,
        ))
    }

    /// The fetcher of an order that takes the package's release, and the word the configured
    /// source goes by. The source is not asked: a staged order is allowed on every host, and the
    /// fetch child is not used.
    fn assemble_seed(&self) -> Result<(SeedFetcher, &'static str), NotBuilt> {
        let (parts, toml, settings) = self.read()?;
        let policy = Policy::of(&toml);
        let seed = SeedFetcher::new(self.host.clone(), settings, policy, parts, &self.build);
        Ok((seed, toml.source.word()))
    }
}

impl Fetcher for HostFetcher {
    fn fetch(&self, rec: &ReleaseLocator) -> Result<InstalledRelease, FetchError> {
        match rec.source {
            OrderSource::Fetch => self.build()?.fetch(rec),
            OrderSource::Staged => self.build_seed()?.fetch(rec),
        }
    }

    fn fetch_activation(
        &self,
        rec: &ReleaseLocator,
        expected: &Facts,
    ) -> Result<InstalledRelease, FetchError> {
        match rec.source {
            OrderSource::Fetch => self.build()?.fetch_activation(rec, expected),
            OrderSource::Staged => self.build_seed()?.fetch_activation(rec, expected),
        }
    }

    fn seed(&self) -> Result<SeedFacts, FetchError> {
        self.build_seed()?.seed()
    }

    /// The package is looked at whatever the source is, and the source is told whether or not
    /// the package carries a seed: the frontend needs the second to know a host fetches nothing.
    fn look(&self, known: Option<&Looked>) -> Noticed {
        match self.assemble_seed() {
            Ok((seed, source)) => Noticed {
                source: Some(source),
                seed: seed.scan(known),
            },
            Err(e) => Noticed {
                source: None,
                seed: Err(FetchError::from(e)),
            },
        }
    }

    /// The word `installer.toml` holds for the source; `None` when the host's parts or the file
    /// cannot be read.
    fn source(&self) -> Option<&'static str> {
        self.assemble_seed().ok().map(|(_, word)| word)
    }

    /// Any installed release is asked, on every kind of host: the seed fetcher has the runner
    /// and the settings, and no source of its own to be denied.
    fn probe(&self, commit: &CommitId) -> Result<Version, FetchError> {
        self.build_seed()?.probe(commit)
    }
}

/// Why no fetcher could be made for this fetch.
#[derive(Debug, thiserror::Error)]
enum NotBuilt {
    /// The host could not supply its parts (no fetch user, for one).
    #[error("{0}")]
    Host(String),
    #[error("{0}")]
    Config(#[from] ReadError),
    #[error("{0}")]
    Settings(Refusal),
    #[error("{0}")]
    Refs(#[from] NoCommonRef),
    #[error("{0}")]
    Source(&'static str),
    /// The frontend source is set and a piece it needs is not there or is not trusted, said
    /// without the bytes of any file.
    #[error("{0}")]
    Pull(String),
    /// The host's configuration does not allow what the order asks.
    #[error("{0}")]
    Denied(&'static str),
}

impl From<NotBuilt> for FetchError {
    fn from(e: NotBuilt) -> FetchError {
        match e {
            NotBuilt::Denied(why) => FetchError::Order {
                word: "denied",
                why: why.to_owned(),
            },
            other => FetchError::Unavailable(other.to_string()),
        }
    }
}

/// The parts of the host `host` itself, with the fetch child `child` and the lease source
/// `leases`: the real executor under the host's root, the real runner, and this process's
/// effective ids as the owner of what a fetch makes (root on a host).
pub fn host_parts(host: &Layout, leases: Box<dyn LeaseSource>, child: ChildProgram) -> Parts {
    Parts {
        trust: Box::new(HostExec::new(host.root().clone())),
        runner: Box::new(SystemRunner),
        leases,
        owner: Owner {
            uid: rustix::process::geteuid().as_raw(),
            gid: rustix::process::getegid().as_raw(),
        },
        child,
    }
}

/// Where `installer.toml` says the bytes come from.
///
/// The `url` source is a base the file validated, with the system's roots. `installer.toml`
/// can now hold the keys of the other sources (`release_ca`, the frontend snapshot), and this
/// build does not use `release_ca`, so a source that needs it is refused, with the reason,
/// rather than built without it: a `url` host with a `release_ca` would otherwise trust the
/// system's roots where the operator named a CA. A `bundled` host fetches nothing: an order
/// that fetches is denied.
///
/// The `frontend` source is built and held to every guard ([`frontend_source`]), and is then
/// refused all the same: this build does not fetch from the frontend yet, because LAN pull
/// waits for a decision on lending the node key to the fetch child. The built source is
/// dropped, so no node key is read or lent. Fetching is one change here: the arm returns
/// `frontend_source(toml, trust).map(Source::Frontend)`.
fn source_of(toml: &InstallerToml, trust: &dyn Exec) -> Result<Source, NotBuilt> {
    match (&toml.source, &toml.release_ca) {
        (Configured::Url(base), None) => Ok(Source::Url {
            base: base.clone(),
            ca_file: None,
        }),
        (Configured::Url(_), Some(_)) => Err(NotBuilt::Source(
            "release_ca is set and this build does not use it yet: a fetch would trust the system's roots instead of the CA named; remove release_ca",
        )),
        (Configured::Frontend, _) => {
            frontend_source(toml, trust)?;
            Err(NotBuilt::Source(
                "release_source = \"frontend\": the snapshot, the node_cert and node_key names and frontend_ca check out, but this build does not fetch from the frontend yet (LAN pull waits for a decision on lending the node key to the fetch child); use release_source = \"url\"",
            ))
        }
        (Configured::Bundled, _) => Err(NotBuilt::Denied(
            "release_source is bundled; source=fetch is refused (use source=staged)",
        )),
    }
}

/// The frontend a fetch would use, built from `installer.toml` after every guard it has.
/// Nothing here reads the node's certificate or key: they are paths, lent to the fetch child
/// only when a fetch is made from the source built.
///
/// In this order, each refusal with its own reason and none holding a file's bytes:
///
/// 1. the snapshot (`frontend_url`, `frontend_ca`, `frontend_ca_sha256`) is in the file;
/// 2. the node's certificate and key are named;
/// 3. the CA copy is a file root alone can write, with no link on its way
///    ([`trusted_chain`]), before any byte of it is asked for;
/// 4. its size is within what the identity reader accepts (at least 1 byte, at most
///    [`MAX_PEM_BYTES`](crate::fetch::identity::MAX_PEM_BYTES)), checked before it is read;
/// 5. its sha256 is the `frontend_ca_sha256` of the file.
///
/// The executor `trust` is rooted at the host's root, and the fetch child opens the paths it
/// is given as they are, so the two agree only when the root is `/`, which is the host's.
///
/// Known residual: the identity reader opens the CA again, a moment after step 5. Only root
/// can write the path under step 3, so this is accepted; lending the bytes just checked would
/// change [`FrontendSource`].
fn frontend_source(toml: &InstallerToml, trust: &dyn Exec) -> Result<FrontendSource, NotBuilt> {
    let Some(pull) = &toml.frontend else {
        return Err(NotBuilt::Source(
            "release_source = \"frontend\" needs frontend_url, frontend_ca and frontend_ca_sha256 (setup: --frontend-worker-url, --frontend-ca, --frontend-ca-sha256)",
        ));
    };
    let (Some(node_cert), Some(node_key)) = (&toml.node_cert, &toml.node_key) else {
        return Err(NotBuilt::Source(
            "release_source = \"frontend\" needs node_cert and node_key: this host is not an enrolled daemon or storage node",
        ));
    };
    let ca = HostPath::new(&pull.ca.to_string_lossy())
        .map_err(|e| NotBuilt::Pull(format!("frontend_ca: {e}")))?;
    trusted_chain(trust, &ca, Kind::File)
        .map_err(|e| NotBuilt::Pull(e.to_string()))?;
    let cap = crate::fetch::identity::MAX_PEM_BYTES;
    let len = trust.stat(&ca).ok().flatten().map_or(0, |m| m.len);
    if !(1..=cap).contains(&len) {
        let why = format!("frontend_ca is empty or over {cap} bytes");
        return Err(NotBuilt::Pull(why));
    }
    let Ok(Some(Entry::File { bytes, .. })) = trust.inspect(&ca) else {
        return Err(NotBuilt::Source("frontend_ca could not be read"));
    };
    if hex::encode(Sha256::digest(bytes)) !=pull.ca_sha256.as_str() {
        return Err(NotBuilt::Source(
            "frontend_ca does not have the sha256 frontend_ca_sha256 names",
        ));
    }
    Ok(FrontendSource {
        url: pull.url.clone(),
        ca_file: pull.ca.clone(),
        node_cert: node_cert.clone(),
        node_key: node_key.clone(),
    })
}

#[cfg(test)]
#[path = "fetcher_host_tests.rs"]
mod tests;
